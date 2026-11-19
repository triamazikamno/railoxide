//! The transactions a Public account sends for a swap it pays on another chain, and the
//! deposit's hand-off read back from that chain.
//!
//! The swap's record is on the destination chain's owner, which sends these on the origin
//! chain through that chain's configuration. Each transaction is in the record before it is
//! broadcast, and its inclusion comes from the step that sent it. A deposit is recorded with
//! its signed bytes too. One that no RPC accepted is left to tracking without a receipt.
//! The hand-off is read from finalized whole-block receipts. A deposit without a finalized
//! inclusion is found by a bounded scan of finalized blocks, matching recorded hashes locally
//! without transaction-hash RPC queries. It is ruled out once the scan passed every block it
//! could be in and the Public account's nonce passed its own. Until then tracking resends the
//! recorded bytes of one without any inclusion unchanged.
//!
//! An order's Public account sends two more: the withdrawal of proceeds its cow-shed proxy
//! holds, and the order's early invalidation. Neither touches the order's own hook batch.

use std::time::SystemTime;

use alloy::eips::{BlockId, BlockNumHash};
use alloy::network::AnyRpcBlock;
use alloy::network::primitives::{HeaderResponse as _, ReceiptResponse as _};
use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::providers::{DynProvider, EthGetBlock, Provider as _};
use alloy::rpc::types::{Filter, Log, TransactionRequest};
use alloy::sol_types::{SolCall as _, SolEvent as _};
use broadcaster_core::contracts::across::{
    SpokePool, address_to_bytes32, private_deposit_calldata,
};
use broadcaster_core::contracts::cow::invalidate_order_calldata;
use broadcaster_core::contracts::cow_shed::{
    ExecuteHooks, decode_withdrawal_calls, execute_hooks_calldata, execute_hooks_digest,
    proxy_address, withdrawal_calls,
};
use broadcaster_core::contracts::executor::AcrossPrivateDelivery;
use broadcaster_core::contracts::shield::build_approve_calldata;
use eyre::{Result, WrapErr as _, eyre};
use railgun_wallet::tx::RailgunGasModel;
use tracing::Instrument as _;
use zeroize::Zeroizing;

use super::destination::{finalized_receipts, still_canonical};
use super::is_live_swap_use;
use super::observation::MAX_OBSERVATION_BLOCKS;
use super::order::valid_to_after;
use super::public_order::{execute_hooks_typed_data, new_public_swap_batch_nonce};
use super::public_settlement::{PublicSwapOrderState, public_swap_order_state, unix_now};
use crate::block_observer::fetch_checked_block_receipts;
use crate::cow::CowOrderbookClient;
use crate::desktop::executor_observation::{ObservationEndpoints, trace_step};
use crate::public_wallet::{
    PublicActionStepEnd, PublicSwapStepOutcome, VaultedPublicSigner,
    admitted_public_signer_authorized, public_native_action_gas_units_with_buffer,
    query_erc20_allowance, query_erc20_balance, sign_public_swap_typed_data,
    submit_public_swap_step,
};
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    AcrossOrderTerms, ExecutorOperationId, PublicSwapDeposited, PublicSwapInclusion,
    PublicSwapObservations, PublicSwapPath, PublicSwapRecord, PublicSwapSignedTransaction,
    PublicSwapTransaction, PublicSwapTransactionKind, SwapBridgeHandoff, SwapObservation,
    SwapUseId, SwapUseRecord, SwapUseRole,
};
use crate::{
    ClassRoute, DesktopPrivateSpendAuthorization, ExecutorOwner, HardwareTrezorPinMatrixProvider,
    HttpContext, OperationHttpClient, PublicActionGasFeeSelection, PublicActionProgressStep,
    PublicActionProgressUpdate, PublicAssetId, RpcRead, RpcRoute, RpcSubmission, WalletRpcOrigin,
    query_rpc_pool_for_route, self_broadcast_send_raw_transaction_to_rpc_pool_with_logging,
};

mod funding;
mod handoff_observation;
mod sending;
mod withdrawal_invalidation;

/// A read of the chain the swap pays on that no endpoint answered.
const ORIGIN_UNAVAILABLE: &str = "the network the swap pays on is unavailable";

/// Gas units of a `SpokePool.depositV3` a Public account sends with a private-delivery message.
/// Fork measurements on chains 1, 56, 137 and 42161 on 2026-10-06 used 84,546 to 101,372 gas.
pub const PUBLIC_ACROSS_DEPOSIT_GAS_UNITS: u64 = 150_000;

/// Gas units of a withdrawal from a cow-shed proxy that has no code yet: the Public account's
/// `COWShedFactory.executeHooks` deploys the proxy, then runs the one token transfer. A fork
/// measurement on 2026-10-06 used 335,199 gas; this is an upper bound above it.
pub const PUBLIC_PROXY_DEPLOYING_WITHDRAWAL_GAS_UNITS: u64 = 400_000;

/// Gas units of a withdrawal from a cow-shed proxy that already has code. It was not measured
/// on its own: it is an upper bound for the same call without the deployment, which is most of
/// the 335,199 gas measured with it.
pub const PUBLIC_PROXY_WITHDRAWAL_GAS_UNITS: u64 = 120_000;

/// What the Public account's own transactions for a swap can cost at most, and the limits that
/// maximum assumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicSwapGasPlan {
    /// One gas limit per approval transaction of the swap's `PublicSwapApprovalPlan`, in order.
    pub approval_gas_limits: Vec<u64>,
    /// The deposit's gas limit. `None` for an order, which the Public account doesn't send.
    pub deposit_gas_limit: Option<u64>,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    /// Sum of the limits times `max_fee_per_gas`, in wei.
    pub max_gas_cost: U256,
}

/// Pure: the plan for `approvals` approval transactions and an optional deposit on `chain` at the
/// given fee.
pub fn public_swap_gas_plan(
    chain: &EffectiveChainConfig,
    approvals: usize,
    deposit: bool,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> Result<PublicSwapGasPlan> {
    let overflow = || eyre!("the swap's gas plan overflows");
    // The limit an approval step of any Public action gets on this chain.
    let approval_gas_limit = public_native_action_gas_units_with_buffer(
        RailgunGasModel::for_chain(chain.chain_id),
        &[PublicActionProgressStep::Approve],
        chain.gas.gas_limit_buffer,
    );
    let approval_gas_limits = vec![approval_gas_limit; approvals];
    let deposit_gas_limit = if deposit {
        Some(
            PUBLIC_ACROSS_DEPOSIT_GAS_UNITS
                .checked_add(chain.gas.gas_limit_buffer)
                .ok_or_else(overflow)?,
        )
    } else {
        None
    };
    let gas_units = approval_gas_limits
        .iter()
        .chain(&deposit_gas_limit)
        .try_fold(U256::ZERO, |sum, limit| sum.checked_add(U256::from(*limit)))
        .ok_or_else(overflow)?;
    let max_gas_cost = gas_units
        .checked_mul(U256::from(max_fee_per_gas))
        .ok_or_else(overflow)?;
    Ok(PublicSwapGasPlan {
        approval_gas_limits,
        deposit_gas_limit,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        max_gas_cost,
    })
}

/// The deposit id and amounts of the `FundsDeposited` event among `logs` that is this swap's
/// hand-off: emitted by `spoke_pool`, naming `source` as depositor and carrying the signed
/// recipient, message hash, destination chain and tokens, an input of at least the signed input
/// and an output of at least the signed output.
pub(crate) fn public_swap_handoff(
    logs: &[Log],
    spoke_pool: Address,
    source: Address,
    destination_chain: u64,
    terms: &AcrossOrderTerms,
) -> Option<(U256, PublicSwapDeposited)> {
    // These swaps always sign a recipient and a message, so terms without them match nothing.
    let (recipient, message_hash) = (terms.recipient?, terms.message_hash?);
    logs.iter()
        .filter(|log| log.address() == spoke_pool)
        .filter_map(|log| log.log_decode::<SpokePool::FundsDeposited>().ok())
        .map(|log| log.inner.data)
        .find(|deposit| {
            deposit.depositor == address_to_bytes32(source)
                && deposit.recipient == address_to_bytes32(recipient)
                && keccak256(&deposit.message) == message_hash
                && deposit.destinationChainId == U256::from(destination_chain)
                && deposit.inputToken == address_to_bytes32(terms.input_token)
                && deposit.outputToken == address_to_bytes32(terms.output_token)
                && deposit.inputAmount >= terms.input_amount
                && deposit.outputAmount >= terms.output_amount
        })
        .map(|deposit| {
            (
                deposit.depositId,
                PublicSwapDeposited {
                    input_amount: deposit.inputAmount,
                    output_amount: deposit.outputAmount,
                },
            )
        })
}

/// The Public account of a swap, and how to authorize its signatures.
pub struct PublicSwapSource<'a> {
    pub public_account_uuid: &'a str,
    pub authorization: Option<&'a DesktopPrivateSpendAuthorization>,
    pub trezor_app_passphrase: Option<Zeroizing<String>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
}

/// An admitted Public source retained through a swap's approvals and signing.
pub struct AuthorizedPublicSwapSource {
    signer: VaultedPublicSigner,
}

impl AuthorizedPublicSwapSource {
    #[must_use]
    pub fn address(&self) -> Address {
        self.signer.address()
    }

    pub(super) const fn signer(&self) -> &VaultedPublicSigner {
        &self.signer
    }
}

/// What sending a swap's approvals came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicSwapApprovalsOutcome {
    /// Nothing is left to send: the allowance covers the sale, the approved order signs a
    /// permit, or the approvals the review priced were sent and succeeded.
    Ready,
    /// The approval plan differs from the reviewed one. Nothing was sent, and the saved
    /// approval is unchanged.
    ReviewRequired {
        /// Whether the account must now send transactions the approval doesn't cover, which
        /// [`ExecutorOwner::requote_public_swap_approvals`] prices. Otherwise it can now sign
        /// a permit the order wasn't priced with, and the swap is quoted afresh.
        transactions: bool,
    },
}

/// What became of a deposit, a withdrawal or an invalidation a swap's Public account sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicSwapTransactionOutcome {
    /// Included and succeeded in this block. What it changed is read once the block is final.
    Included {
        block_number: u64,
        transaction_hash: B256,
    },
    /// Included and reverted: it changed nothing.
    Reverted {
        block_number: u64,
        transaction_hash: B256,
    },
    /// A deposit recorded and sent, which no RPC accepted. Its receipt isn't awaited:
    /// tracking finds its inclusion, resends it, or rules it out.
    HandedOff { transaction_hash: B256 },
}

/// How far sending one of the swap's transactions followed it.
enum PublicSwapSent {
    /// Its receipt was observed and its inclusion recorded.
    Included(PublicSwapStepOutcome),
    /// It was recorded and sent, and no RPC accepted it.
    HandedOff(B256),
}

/// What withdrawing the proceeds held by the proxy would send and cost, read now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicSwapWithdrawalReview {
    /// The Public account's cow-shed proxy.
    pub proxy: Address,
    /// The token the order bought.
    pub token: Address,
    /// The proxy's whole balance of `token`.
    pub amount: U256,
    /// Whether the proxy has code. The withdrawal deploys it otherwise.
    pub proxy_deployed: bool,
    pub gas_limit: u64,
    /// `gas_limit` times the maximum fee per gas, in wei.
    pub max_gas_cost: U256,
}

/// A swap paid from a Public account, as the destination account's use records it.
pub(super) struct ClaimedPublicSwap {
    /// The Public account that pays.
    pub(super) source: Address,
    pub(super) destination_token: Address,
    /// The destination stealth account, once derived.
    pub(super) destination: Option<Address>,
    pub(super) swap: PublicSwapRecord,
    /// Whether the use still claims its account and was not stopped.
    pub(super) live: bool,
    /// The origin chain's pinned `SpokePool`.
    pub(super) spoke_pool: Address,
}

/// What identifies a swap's hand-off among the origin chain's deposits.
pub(super) struct ExpectedHandoff<'a> {
    pub(super) spoke_pool: Address,
    pub(super) source: Address,
    pub(super) destination_chain: u64,
    pub(super) terms: &'a AcrossOrderTerms,
}

impl ExpectedHandoff<'_> {
    pub(super) fn matches(&self, logs: &[Log]) -> Option<(U256, PublicSwapDeposited)> {
        public_swap_handoff(
            logs,
            self.spoke_pool,
            self.source,
            self.destination_chain,
            self.terms,
        )
    }
}

/// A hand-off verified in a finalized block's receipts.
pub(super) struct ObservedHandoff {
    pub(super) block: BlockNumHash,
    pub(super) transaction_hash: B256,
    pub(super) deposit_id: U256,
    pub(super) deposited: PublicSwapDeposited,
}

/// What a query for the Public account's deposits over finalized blocks came to.
pub(super) enum DepositorSearch {
    Found(ObservedHandoff),
    /// No block of the range holds the hand-off.
    Absent,
    /// A block that may hold the hand-off was not final or left the canonical chain during
    /// the read, so the range is not settled.
    Unresolved,
}

/// What a finalized block shows of one deposit transaction.
enum HandoffAt {
    /// The block isn't final yet, or left the canonical chain during the read.
    NotFinal,
    /// The final block doesn't hold the transaction succeeding with the swap's deposit.
    Absent,
    Found(ObservedHandoff),
}

/// Refuse a signer other than the swap's Public account, and a swap that was stopped.
pub(super) fn require_public_swap_source(
    signer: &VaultedPublicSigner,
    claimed: &ClaimedPublicSwap,
) -> Result<()> {
    require_public_swap_signer(signer, claimed)?;
    if !claimed.live {
        return Err(eyre!("this swap was stopped"));
    }
    Ok(())
}

/// Refuse a signer other than the swap's Public account. A withdrawal and an invalidation ask
/// no more: proceeds the proxy holds are the user's whatever became of the swap, and a stopped
/// swap is the one whose open order the user wants gone.
fn require_public_swap_signer(
    signer: &VaultedPublicSigner,
    claimed: &ClaimedPublicSwap,
) -> Result<()> {
    if signer.address() != claimed.source {
        return Err(eyre!(
            "the Public account differs from the one this swap was approved for"
        ));
    }
    Ok(())
}

/// The fee the swap's transactions were reviewed at. They are never sent at a fee the wallet
/// picks while sending, which the approved maximum gas cost wouldn't cover.
fn reviewed_gas_fee(gas_fee: PublicActionGasFeeSelection) -> Result<(u128, u128)> {
    match gas_fee {
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        } => Ok((max_fee_per_gas, max_priority_fee_per_gas)),
        PublicActionGasFeeSelection::Auto => Err(eyre!(
            "a swap's Public account sends its transactions at the reviewed gas fee"
        )),
    }
}

/// Refuse transactions that can cost more than the swap's approval covers. The review covered
/// the transactions the wallet expected then, so a higher need returns to it.
fn require_approved_gas_cost(plan: &PublicSwapGasPlan, approved: U256) -> Result<()> {
    if plan.max_gas_cost > approved {
        return Err(eyre!(
            "the Public account's transactions can cost up to {} wei in gas, more than the {approved} wei approved; review the swap again",
            plan.max_gas_cost
        ));
    }
    Ok(())
}

/// The origin chain's head block number, a broker read over the swap's client.
async fn origin_head(
    origin: &EffectiveChainConfig,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<u64> {
    let route = origin
        .rpc_route
        .for_class(crate::RpcTrafficClass::Sensitive)?
        .isolated(client.clone());
    let head = crate::staking::fetch_latest_block(&route, http, WalletRpcOrigin::PublicWallet)
        .await
        .wrap_err(ORIGIN_UNAVAILABLE)?;
    Ok(head.header.num_hash().number)
}

/// The identity of the origin chain's block `number`, from the first of its endpoints that
/// serves it.
async fn origin_block(
    origin: &EffectiveChainConfig,
    http: &HttpContext,
    client: &OperationHttpClient,
    number: u64,
) -> Result<BlockNumHash> {
    let endpoints =
        ObservationEndpoints::new(origin, http, crate::RpcTrafficClass::Sensitive, client)?;
    let mut attempts = endpoints.attempts();
    while let Some(endpoint) = attempts.next().await {
        match endpoint.provider.get_block_by_number(number.into()).await {
            Ok(Some(block)) if block.header.num_hash().number == number => {
                endpoints.succeeded(&endpoint);
                return Ok(block.header.num_hash());
            }
            // An endpoint that hasn't reached the block yet answered all the same.
            Ok(_) => endpoints.succeeded(&endpoint),
            Err(error) => endpoints.failed(&endpoint, &eyre::Report::from(error)),
        }
    }
    Err(eyre!(
        "the block that includes the Public account's transaction is unavailable"
    ))
}

/// The code at `account` on the chain `route` reads, a broker read.
pub(super) async fn code_at(
    route: &ClassRoute,
    http: &HttpContext,
    account: Address,
) -> Result<Bytes> {
    let read = async {
        let code = http
            .rpc_broker()
            .submit(RpcSubmission::new(
                RpcRoute::from(route.clone()),
                vec![RpcRead::get_code_at(account, BlockId::latest())],
                WalletRpcOrigin::PublicWallet.into(),
            ))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| eyre!("the code read returned nothing"))??;
        Ok::<_, eyre::Report>(serde_json::from_value(code.into_value())?)
    };
    read.await.wrap_err(ORIGIN_UNAVAILABLE)
}

/// Whether `proxy` has code on the chain `route` reads.
pub(super) async fn proxy_deployed(
    route: &ClassRoute,
    http: &HttpContext,
    proxy: Address,
) -> Result<bool> {
    Ok(!code_at(route, http, proxy).await?.is_empty())
}

/// Where the step included the transaction it sent, and whether it succeeded.
fn transaction_outcome(outcome: &PublicSwapStepOutcome) -> Result<PublicSwapTransactionOutcome> {
    let (block_number, transaction_hash) = (
        outcome.receipt.block_number,
        outcome.receipt.tx_hash.parse()?,
    );
    Ok(if outcome.receipt.status {
        PublicSwapTransactionOutcome::Included {
            block_number,
            transaction_hash,
        }
    } else {
        PublicSwapTransactionOutcome::Reverted {
            block_number,
            transaction_hash,
        }
    })
}

/// The proxy and the bought token of a swap whose proxy holds its order's proceeds. Any other
/// swap is refused: only held proceeds are withdrawn.
fn held_proceeds(swap: &PublicSwapRecord) -> Result<(Address, Address)> {
    let order = swap
        .order()
        .ok_or_else(|| eyre!("this swap has no signed order"))?;
    if !matches!(
        public_swap_order_state(swap),
        Some(PublicSwapOrderState::HeldByProxy { .. })
    ) {
        return Err(eyre!("this swap's proxy holds no proceeds to withdraw"));
    }
    Ok((order.proxy(), order.buy_token()))
}

struct DepositRead {
    handoff: Option<ObservedHandoff>,
    updates: Vec<DepositUpdate>,
}

struct DepositUpdate {
    hash: B256,
    inclusion: Option<PublicSwapInclusion>,
    next_block: Option<u64>,
    /// The finalized block that rules the deposit out.
    ruled_out: Option<SwapObservation>,
}

/// Resume each handed-off deposit independently. Inspect at most eight finalized blocks per
/// poll, matching transaction hashes locally before fetching checked whole-block receipts.
///
/// A deposit without an inclusion, or whose provisional inclusion this poll found missing
/// from its now final block, is ruled out when this poll's scan reached a block without
/// finding it, every block from its boundary up to that one having been inspected, and the
/// Public account's nonce at that block, read by its hash as canonical, passed the
/// deposit's own while the block stayed canonical. Another transaction then used the
/// deposit's nonce, so it can never be included. A provisional inclusion not yet final isn't
/// checked, so a deposit awaiting finality costs no nonce reads. A read that fails rules
/// nothing out.
async fn read_public_swap_handoff(
    provider: &DynProvider,
    finality_depth: u64,
    deposits: &[&PublicSwapTransaction],
    expected: &ExpectedHandoff<'_>,
) -> Result<DepositRead> {
    let mut read = DepositRead {
        handoff: None,
        updates: Vec::new(),
    };
    let head = trace_step("public_swap_deposit_head", provider.get_block_number()).await?;
    let Some(finalized_head) = head.checked_sub(finality_depth) else {
        return Ok(read);
    };
    let mut remaining = 8;
    for deposit in deposits {
        if deposit
            .inclusion
            .is_some_and(|inclusion| inclusion.finalized && !inclusion.succeeded)
        {
            continue;
        }
        // A saved first receipt is a location hint, never proof of failure. Verify the
        // canonical block at its number before falling back to the persisted scan boundary.
        // A provisional one that block no longer holds was orphaned by a reorganization.
        let orphaned = if let Some(inclusion) = deposit.inclusion
            && inclusion.observation.block.number <= finalized_head
            && remaining > 0
        {
            remaining -= 1;
            let (_, verified, handoff) = read_deposit_block(
                provider,
                inclusion.observation.block.number,
                deposit.hash,
                expected,
            )
            .await?;
            if let Some(inclusion) = verified {
                read.updates.push(DepositUpdate {
                    hash: deposit.hash,
                    inclusion: Some(inclusion),
                    next_block: None,
                    ruled_out: None,
                });
                if handoff.is_some() {
                    read.handoff = handoff;
                    return Ok(read);
                }
                continue;
            }
            !inclusion.finalized
        } else {
            false
        };
        let mut number = deposit
            .deposit_scan_from_block
            .ok_or_else(|| eyre!("a handed-off deposit has no scan boundary"))?;
        let mut update = DepositUpdate {
            hash: deposit.hash,
            inclusion: None,
            next_block: None,
            ruled_out: None,
        };
        // The last block this poll inspected that doesn't hold the deposit.
        let mut scanned = None;
        while remaining > 0 && number <= finalized_head {
            remaining -= 1;
            let (block, inclusion, handoff) =
                read_deposit_block(provider, number, deposit.hash, expected).await?;
            update.inclusion = inclusion;
            update.next_block = number.checked_add(1);
            if handoff.is_some() {
                read.handoff = handoff;
                break;
            }
            if inclusion.is_some() {
                break;
            }
            scanned = Some(block);
            let Some(next) = number.checked_add(1) else {
                break;
            };
            number = next;
        }
        if (deposit.inclusion.is_none() || orphaned)
            && update.inclusion.is_none()
            && let (Some(block), Some(nonce)) = (scanned, deposit.transaction.nonce)
            && deposit_nonce_used(provider, expected.source, block, nonce).await
        {
            update.ruled_out = Some(SwapObservation {
                block,
                transaction_hash: None,
            });
        }
        if update.next_block.is_some() || update.inclusion.is_some() {
            read.updates.push(update);
        }
        if read.handoff.is_some() || remaining == 0 {
            break;
        }
    }
    Ok(read)
}

/// Whether the Public account `source` had used the nonce `nonce` by the canonical block
/// `block`: its transaction count there, read by the block's hash as canonical, is above it,
/// and the block is still canonical after the read. A read that fails says no.
async fn deposit_nonce_used(
    provider: &DynProvider,
    source: Address,
    block: BlockNumHash,
    nonce: u64,
) -> bool {
    let Ok(count) = trace_step("public_swap_deposit_nonce", async {
        provider
            .get_transaction_count(source)
            .block_id(BlockId::hash_canonical(block.hash))
            .await
    })
    .await
    else {
        return false;
    };
    count > nonce && still_canonical(provider, block).await.unwrap_or(false)
}

/// Verify both successful and failed receipts. A successful deposit must carry the approved
/// terms; a missing or inconsistent receipt never advances the durable scan cursor. Returns
/// the block read with what it holds of the deposit.
async fn read_deposit_block(
    provider: &DynProvider,
    number: u64,
    hash: B256,
    expected: &ExpectedHandoff<'_>,
) -> Result<(
    BlockNumHash,
    Option<PublicSwapInclusion>,
    Option<ObservedHandoff>,
)> {
    let block = trace_step("public_swap_deposit_block", async {
        EthGetBlock::<AnyRpcBlock>::by_number(number.into(), provider.client()).await
    })
    .await?
    .ok_or_else(|| eyre!("Public deposit evidence block is unavailable"))?;
    let identity = block.header.num_hash();
    if identity.number != number {
        return Err(eyre!("Public deposit evidence block has the wrong number"));
    }
    let hashes: Vec<_> = block.transactions.hashes().collect();
    let mut inclusion = None;
    let mut handoff = None;
    if hashes.contains(&hash) {
        let receipts = trace_step(
            "public_swap_deposit_block_receipts",
            fetch_checked_block_receipts(provider, identity, &hashes),
        )
        .await?;
        let receipt = receipts
            .iter()
            .find(|receipt| receipt.transaction_hash() == hash)
            .ok_or_else(|| eyre!("Public deposit receipt is missing"))?;
        if receipt.inner.logs().iter().any(|log| {
            log.removed
                || log.block_hash != Some(identity.hash)
                || log.block_number != Some(number)
                || log.transaction_hash != Some(hash)
        }) {
            return Err(eyre!(
                "Public deposit receipt logs have inconsistent inclusion"
            ));
        }
        if receipt.status() {
            let (deposit_id, deposited) = expected
                .matches(receipt.inner.logs())
                .ok_or_else(|| eyre!("Public deposit receipt differs from the approved terms"))?;
            handoff = Some(ObservedHandoff {
                block: identity,
                transaction_hash: hash,
                deposit_id,
                deposited,
            });
        }
        inclusion = Some(PublicSwapInclusion {
            observation: SwapObservation {
                block: identity,
                transaction_hash: Some(hash),
            },
            succeeded: receipt.status(),
            finalized: true,
        });
    }
    if !still_canonical(provider, identity).await? {
        return Err(eyre!(
            "Public deposit evidence block changed during observation"
        ));
    }
    Ok((identity, inclusion, handoff))
}

/// The `expected` hand-off among the depositor's deposits at the pinned `SpokePool` in the
/// blocks `from..=to`, which must be final. Each log query names the Public account and spans
/// at most [`MAX_OBSERVATION_BLOCKS`] blocks. A log only locates a block: its deposit counts
/// once that block's whole receipts hold it in the transaction the log names. `transactions`
/// limits the search to those hashes, and `None` admits any transaction.
pub(super) async fn find_handoff_by_depositor(
    provider: &DynProvider,
    finality_depth: u64,
    from: u64,
    to: u64,
    transactions: Option<&[B256]>,
    expected: &ExpectedHandoff<'_>,
) -> Result<DepositorSearch> {
    let mut unresolved = false;
    let mut page_from = from;
    loop {
        let page_to = to.min(page_from.saturating_add(MAX_OBSERVATION_BLOCKS - 1));
        let filter = Filter::new()
            .address(expected.spoke_pool)
            .event_signature(SpokePool::FundsDeposited::SIGNATURE_HASH)
            .topic3(address_to_bytes32(expected.source))
            .from_block(page_from)
            .to_block(page_to);
        let logs = trace_step("public_swap_deposit_logs", provider.get_logs(&filter)).await?;
        for log in &logs {
            let (Some(number), Some(hash)) = (log.block_number, log.transaction_hash) else {
                continue;
            };
            if log.removed
                || !(page_from..=page_to).contains(&number)
                || transactions.is_some_and(|transactions| !transactions.contains(&hash))
                || expected.matches(std::slice::from_ref(log)).is_none()
            {
                continue;
            }
            match handoff_at(provider, finality_depth, number, hash, expected).await? {
                HandoffAt::Found(found) => return Ok(DepositorSearch::Found(found)),
                HandoffAt::NotFinal => unresolved = true,
                HandoffAt::Absent => {}
            }
        }
        if page_to >= to {
            return Ok(if unresolved {
                DepositorSearch::Unresolved
            } else {
                DepositorSearch::Absent
            });
        }
        page_from = page_to + 1;
    }
}

/// The hand-off of the transaction `hash` in the finalized receipts of block `number`.
async fn handoff_at(
    provider: &DynProvider,
    finality_depth: u64,
    number: u64,
    hash: B256,
    expected: &ExpectedHandoff<'_>,
) -> Result<HandoffAt> {
    let Some((block, receipts)) = finalized_receipts(provider, finality_depth, number).await?
    else {
        return Ok(HandoffAt::NotFinal);
    };
    let found = receipts
        .iter()
        .find(|receipt| receipt.transaction_hash() == hash)
        .and_then(|receipt| expected.matches(receipt.inner.logs()));
    if !still_canonical(provider, block).await? {
        return Ok(HandoffAt::NotFinal);
    }
    Ok(found.map_or(HandoffAt::Absent, |(deposit_id, deposited)| {
        HandoffAt::Found(ObservedHandoff {
            block,
            transaction_hash: hash,
            deposit_id,
            deposited,
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPOKE_POOL: Address = Address::repeat_byte(0x11);
    const SOURCE: Address = Address::repeat_byte(0x50);
    const HANDLER: Address = Address::repeat_byte(0x12);
    const MESSAGE: &[u8] = b"private delivery";
    const DESTINATION_CHAIN: u64 = 137;

    fn terms() -> AcrossOrderTerms {
        AcrossOrderTerms {
            spoke_pool: SPOKE_POOL,
            input_token: Address::repeat_byte(6),
            output_token: Address::repeat_byte(10),
            input_amount: U256::from(1_000),
            output_amount: U256::from(990),
            quote_timestamp: 1_700_000_000,
            fill_deadline: 1_700_021_600,
            exclusive_relayer: Address::ZERO,
            exclusivity_parameter: 0,
            recipient: Some(HANDLER),
            message_hash: Some(keccak256(MESSAGE)),
        }
    }

    /// The event of the deposit `terms` signs, with deposit id 77.
    fn deposited() -> SpokePool::FundsDeposited {
        let terms = terms();
        SpokePool::FundsDeposited {
            inputToken: address_to_bytes32(terms.input_token),
            outputToken: address_to_bytes32(terms.output_token),
            inputAmount: terms.input_amount,
            outputAmount: terms.output_amount,
            destinationChainId: U256::from(DESTINATION_CHAIN),
            depositId: U256::from(77),
            quoteTimestamp: terms.quote_timestamp,
            fillDeadline: terms.fill_deadline,
            exclusivityDeadline: 0,
            depositor: address_to_bytes32(SOURCE),
            recipient: address_to_bytes32(HANDLER),
            exclusiveRelayer: address_to_bytes32(terms.exclusive_relayer),
            message: Bytes::from_static(MESSAGE),
        }
    }

    fn handoff(emitter: Address, event: &SpokePool::FundsDeposited) -> Option<(U256, U256, U256)> {
        let log = Log {
            inner: alloy::primitives::Log {
                address: emitter,
                data: event.encode_log_data(),
            },
            ..Log::default()
        };
        public_swap_handoff(&[log], SPOKE_POOL, SOURCE, DESTINATION_CHAIN, &terms())
            .map(|(id, amounts)| (id, amounts.input_amount, amounts.output_amount))
    }

    #[test]
    fn the_gas_plan_counts_every_approval_and_the_deposit() {
        let chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        let buffer = chain.gas.gas_limit_buffer;
        let plan = public_swap_gas_plan(&chain, 2, true, 7, 1).unwrap();
        let [reset, approval] = plan.approval_gas_limits[..] else {
            panic!("two approvals take two limits");
        };
        let deposit = plan.deposit_gas_limit.unwrap();
        assert_eq!(reset, approval);
        assert!(approval > buffer);
        assert_eq!(deposit, PUBLIC_ACROSS_DEPOSIT_GAS_UNITS + buffer);
        assert_eq!(
            (plan.max_fee_per_gas, plan.max_priority_fee_per_gas),
            (7, 1)
        );
        assert_eq!(
            plan.max_gas_cost,
            U256::from(reset + approval + deposit) * U256::from(7)
        );

        // An order's deposit is its hook's, so the Public account pays for approvals only.
        let order = public_swap_gas_plan(&chain, 1, false, 7, 1).unwrap();
        assert_eq!(order.deposit_gas_limit, None);
        assert_eq!(order.max_gas_cost, U256::from(approval) * U256::from(7));
    }

    #[test]
    fn the_handoff_is_the_pinned_pools_deposit_of_the_signed_terms() {
        let signed = deposited();
        assert_eq!(
            handoff(SPOKE_POOL, &signed),
            Some((U256::from(77), U256::from(1_000), U256::from(990)))
        );
        // A deposit of more than the signed amounts is the hand-off, with what it deposited.
        let mut more = deposited();
        more.inputAmount = U256::from(1_001);
        more.outputAmount = U256::from(995);
        assert_eq!(
            handoff(SPOKE_POOL, &more),
            Some((U256::from(77), U256::from(1_001), U256::from(995)))
        );

        let mut other_recipient = deposited();
        other_recipient.recipient = address_to_bytes32(Address::repeat_byte(0x13));
        let mut other_message = deposited();
        other_message.message = Bytes::from_static(b"another delivery");
        let mut short_output = deposited();
        short_output.outputAmount = U256::from(989);
        let mut other_depositor = deposited();
        other_depositor.depositor = address_to_bytes32(Address::repeat_byte(0x51));
        for refused in [
            other_recipient,
            other_message,
            short_output,
            other_depositor,
        ] {
            assert_eq!(handoff(SPOKE_POOL, &refused), None);
        }
        // Anyone can emit the event, so only the pinned pool's counts.
        assert_eq!(handoff(Address::repeat_byte(0x14), &signed), None);

        // Terms without a signed recipient or message match no deposit.
        let log = Log {
            inner: alloy::primitives::Log {
                address: SPOKE_POOL,
                data: signed.encode_log_data(),
            },
            ..Log::default()
        };
        let unsigned = AcrossOrderTerms {
            message_hash: None,
            ..terms()
        };
        assert_eq!(
            public_swap_handoff(&[log], SPOKE_POOL, SOURCE, DESTINATION_CHAIN, &unsigned),
            None
        );
    }

    // An endpoint that hasn't reached the block answers `null`. The next one serves it, and the
    // first isn't withdrawn for it.
    #[tokio::test]
    async fn the_inclusion_block_is_read_from_an_endpoint_that_has_reached_it() {
        use serde_json::{Value, json};
        use std::sync::Arc;
        let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
        block.header.inner.number = 50;
        block.header.hash = B256::repeat_byte(50);
        let block = serde_json::to_value(block).unwrap();
        let (lagging, lagging_requests, lagging_task) =
            crate::rpc_broker::tests::spawn_recording_rpc_mock(Arc::new(
                |request: Value| json!({"jsonrpc": "2.0", "id": request["id"], "result": null}),
            ))
            .await;
        let (serving, serving_task) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request: Value| {
                json!({"jsonrpc": "2.0", "id": request["id"], "result": block})
            }),
            Arc::default(),
            Arc::default(),
        )
        .await;
        let mut origin = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        origin.rpc_route = crate::RpcChainRoute::new(1, vec![lagging, serving]);
        let http = HttpContext::direct_for_tests();
        for reads in 1..=2 {
            assert_eq!(
                origin_block(&origin, &http, OperationHttpClient::direct_for_tests(), 50)
                    .await
                    .unwrap(),
                BlockNumHash::new(50, B256::repeat_byte(50))
            );
            // The broker breaks ties in configured order, so the lagging endpoint is asked first
            // each time.
            assert_eq!(lagging_requests.lock().unwrap().len(), reads);
        }
        lagging_task.abort();
        serving_task.abort();
    }
}
