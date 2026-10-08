//! The transactions a Public account sends for a swap it pays on another chain, and the
//! deposit's hand-off read back from that chain.
//!
//! The swap's record is on the destination chain's owner, which sends these on the origin
//! chain through that chain's configuration. Each transaction is in the record before it is
//! broadcast, and its inclusion comes from the step that sent it. The hand-off is read from
//! finalized whole-block receipts. Interrupted deposits resume a bounded scan of finalized
//! blocks, matching recorded hashes locally without transaction-hash RPC queries.
//!
//! An order's Public account sends two more: the withdrawal of proceeds its cow-shed proxy
//! holds, and the order's early invalidation. Neither touches the order's own hook batch.

use std::time::SystemTime;

use alloy::eips::BlockNumHash;
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
use broadcaster_core::query_rpc_pool::QueryRpcPool;
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
    PublicSwapStepOutcome, VaultedPublicSigner, admitted_public_signer_authorized,
    public_native_action_gas_units_with_buffer, query_erc20_allowance, query_erc20_balance,
    sign_public_swap_typed_data, submit_public_swap_step,
};
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    AcrossOrderTerms, ExecutorOperationId, PublicSwapDeposited, PublicSwapInclusion,
    PublicSwapObservations, PublicSwapPath, PublicSwapRecord, PublicSwapTransaction,
    PublicSwapTransactionKind, SwapBridgeHandoff, SwapObservation, SwapUseId, SwapUseRecord,
    SwapUseRole,
};
use crate::{
    DesktopPrivateSpendAuthorization, ExecutorOwner, HardwareTrezorPinMatrixProvider, HttpContext,
    PublicActionGasFeeSelection, PublicActionProgressStep, PublicActionProgressUpdate,
    PublicAssetId, query_rpc_pool_with_http_client,
};

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

/// The approvals a Public account sends before a swap that sells `amount`: none when `allowance`
/// covers it, the exact approval when the allowance is zero, and a reset to zero first when it is
/// short and not zero. Tokens such as Ethereum's USDT reject a change from one nonzero allowance
/// to another, so the reset applies to every token.
#[must_use]
pub fn public_swap_approvals(allowance: U256, amount: U256) -> Vec<U256> {
    if allowance >= amount {
        Vec::new()
    } else if allowance.is_zero() {
        vec![amount]
    } else {
        vec![U256::ZERO, amount]
    }
}

/// What the Public account's own transactions for a swap can cost at most, and the limits that
/// maximum assumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicSwapGasPlan {
    /// One gas limit per approval transaction `public_swap_approvals` returns, in order.
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

impl ExecutorOwner {
    /// Read the Public account's allowance of the swap's Sell token to `spender` on the origin
    /// chain and plan its transactions at the given fee: for the review, and again before
    /// sending.
    pub async fn plan_public_swap_gas(
        &self,
        origin: &EffectiveChainConfig,
        source: Address,
        sell_token: Address,
        spender: Address,
        amount: U256,
        deposit: bool,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> Result<PublicSwapGasPlan> {
        self.ensure_active()?;
        // The native asset is deposited as value and takes no approval.
        let approvals = if sell_token == Address::ZERO {
            0
        } else {
            let allowance = self
                .public_swap_allowance(origin, source, sell_token, spender)
                .await?;
            public_swap_approvals(allowance, amount).len()
        };
        public_swap_gas_plan(
            origin,
            approvals,
            deposit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
        )
    }

    /// Send the approvals the swap needs from its Public account, each persisted before it is
    /// broadcast.
    pub async fn submit_public_swap_approvals(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        source: &AuthorizedPublicSwapSource,
        gas_fee: PublicActionGasFeeSelection,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<()> {
        Box::pin(self.submit_public_swap_approvals_with_signer(
            operation,
            swap_use,
            origin,
            &source.signer,
            gas_fee,
            progress,
        ))
        .await
    }

    /// [`Self::submit_public_swap_approvals`] with the Public account's signer.
    ///
    /// The allowance read decides what is still needed, so running this again after a restart
    /// sends only the approvals that are missing.
    pub(crate) async fn submit_public_swap_approvals_with_signer(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        mut gas_fee: PublicActionGasFeeSelection,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<()> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        require_public_swap_source(signer, &claimed)?;
        let (approval, intent) = (claimed.swap.approval(), claimed.swap.intent());
        let sell_token = approval.sell_token;
        // The native asset is deposited as value and takes no approval.
        if sell_token == Address::ZERO {
            return Ok(());
        }
        let amount = approval.bounds.sell_amount;
        // An order's Sell token is pulled by `CoW`'s vault relayer, a deposit's by the pool.
        let spender = if intent.order {
            origin
                .public_swap_profile()
                .ok_or_else(|| {
                    eyre!("swaps from a Public account are unavailable on this network")
                })?
                .vault_relayer()
        } else {
            claimed.spoke_pool
        };
        let allowance = self
            .public_swap_allowance(origin, claimed.source, sell_token, spender)
            .await?;
        let values = public_swap_approvals(allowance, amount);
        let mut remaining = values.as_slice();
        let mut nonce = None;
        while let Some((value, later)) = remaining.split_first() {
            let (max_fee_per_gas, max_priority_fee_per_gas) = reviewed_gas_fee(gas_fee)?;
            let plan = public_swap_gas_plan(
                origin,
                remaining.len(),
                !intent.order,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            )?;
            require_approved_gas_cost(&plan, approval.max_gas_cost)?;
            let gas_limit = plan
                .approval_gas_limits
                .first()
                .copied()
                .ok_or_else(|| eyre!("the swap's gas plan has no approval"))?;
            let mut transaction = TransactionRequest::default()
                .to(sell_token)
                .input(Bytes::from(build_approve_calldata(spender, *value)).into());
            transaction.chain_id = Some(origin.chain_id);
            transaction.from = Some(claimed.source);
            transaction.nonce = nonce;
            transaction.gas = Some(gas_limit);
            let kind = if value.is_zero() {
                PublicSwapTransactionKind::ApprovalReset
            } else {
                PublicSwapTransactionKind::Approval
            };
            let outcome = self
                .submit_public_swap_transaction(
                    operation,
                    swap_use,
                    origin,
                    signer,
                    kind,
                    PublicActionProgressStep::Approve,
                    transaction,
                    gas_fee,
                    progress,
                )
                .await?;
            if !outcome.receipt.status {
                return Err(eyre!(
                    "the Public account's approval reverted ({})",
                    outcome.receipt.tx_hash
                ));
            }
            nonce = Some(outcome.next_nonce);
            gas_fee = outcome.gas_fee;
            remaining = later;
        }
        Ok(())
    }

    /// Send the swap's deposit from its Public account, for a signed delivery.
    pub async fn submit_public_swap_deposit(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        source: &AuthorizedPublicSwapSource,
        gas_fee: PublicActionGasFeeSelection,
        delivery: &AcrossPrivateDelivery,
        terms: &AcrossOrderTerms,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapTransactionOutcome> {
        Box::pin(self.submit_public_swap_deposit_with_signer(
            operation,
            swap_use,
            origin,
            &source.signer,
            gas_fee,
            delivery,
            terms,
            progress,
        ))
        .await
    }

    /// [`Self::submit_public_swap_deposit`] with the Public account's signer.
    ///
    /// The path and the signed terms are in the record before the deposit is signed, and the
    /// deposit is in it before it is broadcast.
    pub(crate) async fn submit_public_swap_deposit_with_signer(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        gas_fee: PublicActionGasFeeSelection,
        delivery: &AcrossPrivateDelivery,
        terms: &AcrossOrderTerms,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapTransactionOutcome> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        require_public_swap_source(signer, &claimed)?;
        let (approval, intent) = (claimed.swap.approval(), claimed.swap.intent());
        if intent.order {
            return Err(eyre!(
                "this swap places an order; its Public account sends no deposit"
            ));
        }
        if claimed
            .swap
            .transactions()
            .iter()
            .any(|transaction| transaction.kind == PublicSwapTransactionKind::Deposit)
        {
            return Err(eyre!(
                "this swap's deposit was already handed off; check its progress"
            ));
        }
        if claimed.destination != Some(delivery.destination_executor) {
            return Err(eyre!(
                "the destination stealth account changed; review the swap again"
            ));
        }
        let destination_minimum = approval
            .bounds
            .destination_minimum
            .ok_or_else(|| eyre!("the swap's approval has no destination minimum"))?;
        if terms.spoke_pool != claimed.spoke_pool
            || terms.recipient != Some(delivery.handler)
            || terms.input_token != intent.bridged_token
            || terms.output_token != claimed.destination_token
            || terms.output_amount < destination_minimum
            || terms.input_amount != approval.bounds.sell_amount
        {
            return Err(eyre!(
                "the deposit's terms differ from the approved swap; review the swap again"
            ));
        }
        let (max_fee_per_gas, max_priority_fee_per_gas) = reviewed_gas_fee(gas_fee)?;
        let plan =
            public_swap_gas_plan(origin, 0, true, max_fee_per_gas, max_priority_fee_per_gas)?;
        require_approved_gas_cost(&plan, approval.max_gas_cost)?;
        let gas_limit = plan
            .deposit_gas_limit
            .ok_or_else(|| eyre!("the swap's gas plan has no deposit"))?;

        let deposit = SpokePool::depositV3Call {
            depositor: claimed.source,
            recipient: delivery.handler,
            inputToken: terms.input_token,
            outputToken: terms.output_token,
            inputAmount: terms.input_amount,
            outputAmount: terms.output_amount,
            destinationChainId: U256::from(self.chain.chain_id),
            exclusiveRelayer: terms.exclusive_relayer,
            quoteTimestamp: terms.quote_timestamp,
            fillDeadline: terms.fill_deadline,
            exclusivityParameter: terms.exclusivity_parameter,
            message: Bytes::new(),
        };
        let calldata = private_deposit_calldata(
            deposit,
            delivery.handler,
            delivery.destination_executor,
            delivery.shield_multicall.clone(),
            delivery.fallback,
        )?;
        // The fill is matched on the message Across quoted, so the deposit must carry it.
        let message = SpokePool::depositV3Call::abi_decode(&calldata)?.message;
        if terms.message_hash != Some(keccak256(&message)) {
            return Err(eyre!(
                "the deposit's message differs from the one Across quoted; review the swap again"
            ));
        }
        // The native asset is deposited as its wrapped token, paid as the transaction's value.
        let value = if approval.sell_token == Address::ZERO {
            terms.input_amount
        } else {
            U256::ZERO
        };
        let mut transaction = TransactionRequest::default()
            .to(claimed.spoke_pool)
            .value(value)
            .input(calldata.into());
        transaction.chain_id = Some(origin.chain_id);
        transaction.from = Some(claimed.source);
        transaction.gas = Some(gas_limit);

        self.ensure_active()?;
        self.store
            .record_public_swap_path(operation, swap_use, PublicSwapPath::Deposit, *terms)?;
        self.notify_change();
        let outcome = self
            .submit_public_swap_transaction(
                operation,
                swap_use,
                origin,
                signer,
                PublicSwapTransactionKind::Deposit,
                PublicActionProgressStep::Send,
                transaction,
                gas_fee,
                progress,
            )
            .await?;
        transaction_outcome(&outcome)
    }

    /// Read the deposit's hand-off from finalized whole-block receipts on the origin chain and
    /// record it. `None` while it isn't final or found.
    ///
    /// A deposit with a recorded inclusion is read in that block once it is final. Otherwise
    /// at most eight finalized blocks are inspected per poll from the durable hand-off
    /// boundary, matching the hash locally and saving progress for restart. A stopped swap's
    /// deposit is read too.
    pub async fn observe_public_swap_handoff(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
    ) -> Result<Option<SwapBridgeHandoff>> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        let swap = &claimed.swap;
        if let Some(handoff) = swap.observations().bridge_handoff {
            return Ok(Some(handoff));
        }
        let (Some(PublicSwapPath::Deposit), Some(terms)) = (swap.path(), swap.bridge()) else {
            return Ok(None);
        };
        let deposits: Vec<_> = swap
            .transactions()
            .iter()
            .filter(|transaction| transaction.kind == PublicSwapTransactionKind::Deposit)
            .collect();
        if deposits.is_empty() {
            return Ok(None);
        }
        let expected = ExpectedHandoff {
            spoke_pool: claimed.spoke_pool,
            source: claimed.source,
            destination_chain: self.chain.chain_id,
            terms,
        };
        let endpoints = ObservationEndpoints::new(origin, &self.http);
        for endpoint in endpoints.providers().await {
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = endpoint.index);
            let result = trace_step(
                "public_swap_handoff",
                self.while_active(Box::pin(read_public_swap_handoff(
                    &endpoint.provider,
                    origin.finality_depth,
                    &deposits,
                    &expected,
                ))),
            )
            .instrument(span)
            .await;
            match result {
                Ok(read) => {
                    endpoints.succeeded(&endpoint);
                    for update in read.updates {
                        if let Some(inclusion) = update.inclusion {
                            self.store.record_public_swap_inclusion(
                                operation,
                                swap_use,
                                update.hash,
                                inclusion,
                            )?;
                        }
                        if let Some(next_block) = update.next_block {
                            self.store.record_public_swap_deposit_scan(
                                operation,
                                swap_use,
                                update.hash,
                                next_block,
                            )?;
                        }
                    }
                    self.notify_change();
                    return match read.handoff {
                        Some(found) => self
                            .record_public_swap_handoff(operation, swap_use, &found)
                            .await
                            .map(Some),
                        None => Ok(None),
                    };
                }
                Err(error) => endpoints.failed(&endpoint, &error),
            }
        }
        Err(eyre!(
            "Deposit verification on the network the swap pays on is unavailable. The swap will be checked again."
        ))
    }

    /// Persist a verified hand-off: the inclusion of its transaction, when the record holds
    /// another or none, and the deposit with its amounts beside what was already observed.
    async fn record_public_swap_handoff(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        found: &ObservedHandoff,
    ) -> Result<SwapBridgeHandoff> {
        let _guard = self.lock_activity().await;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("the destination stealth account is unavailable"))?;
        let (_, swap) = record.public_swap_use(swap_use).ok_or_else(|| {
            eyre!("this stealth account isn't claimed by a swap paid from a Public account")
        })?;
        let observation = SwapObservation {
            block: found.block,
            transaction_hash: Some(found.transaction_hash),
        };
        let inclusion = PublicSwapInclusion {
            observation,
            succeeded: true,
            finalized: true,
        };
        if !swap.transactions().iter().any(|transaction| {
            transaction.hash == found.transaction_hash && transaction.inclusion == Some(inclusion)
        }) {
            self.store.record_public_swap_inclusion(
                operation,
                swap_use,
                found.transaction_hash,
                inclusion,
            )?;
        }
        let handoff = SwapBridgeHandoff {
            observation,
            deposit_id: Some(found.deposit_id),
        };
        self.store.record_public_swap_observations(
            operation,
            swap_use,
            PublicSwapObservations {
                bridge_handoff: Some(handoff),
                deposited: Some(found.deposited),
                ..swap.observations()
            },
        )?;
        self.notify_change();
        Ok(handoff)
    }

    /// Read what a withdrawal of the proceeds the swap's proxy holds would send, and plan its
    /// gas at the given fee. The amount is the proxy's whole balance of the bought token now,
    /// and a zero balance is an error. Allowed while the proxy holds the proceeds, before or
    /// after the order's hook batch can no longer run, and for a swap that was stopped.
    pub async fn review_public_swap_withdrawal(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> Result<PublicSwapWithdrawalReview> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        let (proxy, token) = held_proceeds(&claimed.swap)?;
        if max_priority_fee_per_gas > max_fee_per_gas {
            return Err(eyre!(
                "the withdrawal's priority fee is above its maximum fee"
            ));
        }
        let amount = self
            .while_active(query_erc20_balance(
                &origin.rpc_route,
                &self.http,
                token,
                proxy,
            ))
            .await
            .wrap_err("read the proxy's balance of the bought token")?;
        if amount.is_zero() {
            return Err(eyre!("the proxy holds none of the token"));
        }
        let pool = query_rpc_pool_with_http_client(origin.rpc_route.endpoint_urls(), &self.http);
        let proxy_deployed = self.while_active(proxy_deployed(&pool, proxy)).await?;
        let gas_units = if proxy_deployed {
            PUBLIC_PROXY_WITHDRAWAL_GAS_UNITS
        } else {
            PUBLIC_PROXY_DEPLOYING_WITHDRAWAL_GAS_UNITS
        };
        let overflow = || eyre!("the withdrawal's gas plan overflows");
        let gas_limit = gas_units
            .checked_add(origin.gas.gas_limit_buffer)
            .ok_or_else(overflow)?;
        let max_gas_cost = U256::from(gas_limit)
            .checked_mul(U256::from(max_fee_per_gas))
            .ok_or_else(overflow)?;
        Ok(PublicSwapWithdrawalReview {
            proxy,
            token,
            amount,
            proxy_deployed,
            gas_limit,
            max_gas_cost,
        })
    }

    /// Return the bought token from the proxy to the Public account: one transaction from that
    /// account through the factory.
    pub async fn submit_public_swap_withdrawal(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        source: &AuthorizedPublicSwapSource,
        gas_fee: PublicActionGasFeeSelection,
        review: &PublicSwapWithdrawalReview,
        hash_fallback_confirmed: bool,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapTransactionOutcome> {
        Box::pin(self.submit_public_swap_withdrawal_with_signer(
            operation,
            swap_use,
            origin,
            &source.signer,
            gas_fee,
            review,
            hash_fallback_confirmed,
            progress,
        ))
        .await
    }

    /// [`Self::submit_public_swap_withdrawal`] with the Public account's signer.
    ///
    /// The account signs a batch of its own with one call, the token's transfer of the reviewed
    /// amount to the account, under a fresh nonce, and sends it through the factory, which
    /// deploys the proxy when it has no code. A call straight to a proxy without code would
    /// succeed and move nothing. The order's hook batch is left as it was signed. A withdrawal
    /// that loses the race to a late run of that batch reverts.
    pub(crate) async fn submit_public_swap_withdrawal_with_signer(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        gas_fee: PublicActionGasFeeSelection,
        review: &PublicSwapWithdrawalReview,
        hash_fallback_confirmed: bool,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapTransactionOutcome> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        require_public_swap_signer(signer, &claimed)?;
        let (proxy, token) = held_proceeds(&claimed.swap)?;
        let profile = origin
            .public_swap_profile()
            .ok_or_else(|| eyre!("swaps from a Public account are unavailable on this network"))?;
        let source = claimed.source;
        let (factory, implementation) = (
            profile.cow_shed_factory(),
            profile.cow_shed_implementation(),
        );
        if review.proxy != proxy
            || review.token != token
            || proxy != proxy_address(factory, implementation, source)
        {
            return Err(eyre!(
                "the withdrawal differs from this swap's proxy and bought token; review it again"
            ));
        }
        let (max_fee_per_gas, _) = reviewed_gas_fee(gas_fee)?;
        if U256::from(review.gas_limit)
            .checked_mul(U256::from(max_fee_per_gas))
            .is_none_or(|cost| cost > review.max_gas_cost)
        {
            return Err(eyre!(
                "the withdrawal can cost more in gas than reviewed; review it again"
            ));
        }

        let calls = withdrawal_calls(token, source, review.amount)?;
        // The batch about to be signed is read back: it pays only the Public account.
        if decode_withdrawal_calls(&calls, source)? != (token, review.amount) {
            return Err(eyre!(
                "the withdrawal batch differs from the reviewed withdrawal; nothing was signed"
            ));
        }
        let hooks = ExecuteHooks {
            calls,
            nonce: new_public_swap_batch_nonce()?,
            deadline: U256::from(valid_to_after(
                SystemTime::now(),
                profile.valid_to_window(),
            )?),
        };
        let chain_id = origin.chain_id;
        let signature = self
            .while_active(sign_public_swap_typed_data(
                signer,
                execute_hooks_typed_data(&hooks, chain_id, proxy)?,
                execute_hooks_digest(&hooks, chain_id, proxy),
                hash_fallback_confirmed,
            ))
            .await?;
        let calldata =
            execute_hooks_calldata(hooks, source, &signature, chain_id, factory, implementation)?;
        let mut transaction = TransactionRequest::default()
            .to(factory)
            .input(calldata.into());
        transaction.chain_id = Some(chain_id);
        transaction.from = Some(source);
        transaction.gas = Some(review.gas_limit);
        let outcome = self
            .submit_public_swap_transaction(
                operation,
                swap_use,
                origin,
                signer,
                PublicSwapTransactionKind::Withdrawal,
                PublicActionProgressStep::Send,
                transaction,
                gas_fee,
                progress,
            )
            .await?;
        let outcome = transaction_outcome(&outcome)?;
        // The withdrawal counts once its block is final. A read that fails here is repeated by
        // the swap's next observation, so it doesn't fail a withdrawal that was sent.
        if matches!(outcome, PublicSwapTransactionOutcome::Included { .. }) {
            let _ =
                Box::pin(self.advance_public_swap_order(operation, swap_use, origin, None)).await;
        }
        Ok(outcome)
    }

    /// Cancel an open order early: the Public account invalidates it on chain. The hook batch
    /// is never revoked.
    pub async fn submit_public_swap_invalidation(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        source: &AuthorizedPublicSwapSource,
        gas_fee: PublicActionGasFeeSelection,
        orderbook: &CowOrderbookClient,
        max_gas_limit: u64,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapTransactionOutcome> {
        Box::pin(self.submit_public_swap_invalidation_with_signer(
            operation,
            swap_use,
            origin,
            &source.signer,
            gas_fee,
            orderbook,
            max_gas_limit,
            progress,
        ))
        .await
    }

    /// [`Self::submit_public_swap_invalidation`] with the Public account's signer.
    ///
    /// The one transaction is `GPv2Settlement.invalidateOrder` from the account to the pinned
    /// settlement. Nothing is sent to the proxy or the factory: revoking the batch's nonce while
    /// the order can still fill would turn a fill into proceeds the proxy holds, so the batch
    /// stays valid until its deadline. The invalidation succeeds on an order that already
    /// filled, so its inclusion says nothing of the outcome: the order's trade is looked up
    /// afterwards, and the swap counts as cancelled only when the orderbook reports none.
    pub(crate) async fn submit_public_swap_invalidation_with_signer(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        gas_fee: PublicActionGasFeeSelection,
        orderbook: &CowOrderbookClient,
        max_gas_limit: u64,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapTransactionOutcome> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        require_public_swap_signer(signer, &claimed)?;
        // An order past its `validTo` stays open until its end is read, and can't fill.
        if public_swap_order_state(&claimed.swap) != Some(PublicSwapOrderState::Open)
            || !claimed.swap.order_can_fill(unix_now()?)
        {
            return Err(eyre!("only an open order can be cancelled"));
        }
        let order = claimed
            .swap
            .order()
            .ok_or_else(|| eyre!("this swap has no signed order"))?;
        let settlement = origin
            .public_swap_profile()
            .ok_or_else(|| eyre!("swaps from a Public account are unavailable on this network"))?
            .settlement();
        reviewed_gas_fee(gas_fee)?;
        let mut transaction = TransactionRequest::default()
            .to(settlement)
            .input(invalidate_order_calldata(&order.uid()).into());
        transaction.chain_id = Some(origin.chain_id);
        transaction.from = Some(claimed.source);
        transaction.gas = Some(max_gas_limit);
        let outcome = self
            .submit_public_swap_transaction(
                operation,
                swap_use,
                origin,
                signer,
                PublicSwapTransactionKind::Invalidation,
                PublicActionProgressStep::Send,
                transaction,
                gas_fee,
                progress,
            )
            .await?;
        let outcome = transaction_outcome(&outcome)?;
        // A trade decides, and only without one is the swap cancelled. A lookup that fails here
        // is repeated by the swap's next observation, so it doesn't fail an invalidation that
        // was sent.
        if matches!(outcome, PublicSwapTransactionOutcome::Included { .. }) {
            let _ =
                Box::pin(self.observe_public_swap_order(operation, swap_use, origin, orderbook))
                    .await;
        }
        Ok(outcome)
    }

    /// The swap of the use `swap_use`, checked against the chain `origin` it pays on.
    pub(super) fn claimed_public_swap(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
    ) -> Result<ClaimedPublicSwap> {
        self.ensure_active()?;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("the destination stealth account is unavailable"))?;
        let Some(SwapUseRole::PublicSourceDestination {
            origin_chain,
            source,
            destination_token,
            swap,
            ..
        }) = record.swap_use(swap_use).map(SwapUseRecord::role)
        else {
            return Err(eyre!(
                "this stealth account isn't claimed by a swap paid from a Public account"
            ));
        };
        if origin.chain_id != *origin_chain {
            return Err(eyre!(
                "the network the swap pays on differs from the one it was claimed for"
            ));
        }
        let spoke_pool = origin
            .bridge_origin_profile()
            .ok_or_else(|| eyre!("this network has no pinned Across SpokePool"))?
            .spoke_pool();
        Ok(ClaimedPublicSwap {
            source: *source,
            destination_token: *destination_token,
            destination: record.address(),
            swap: (**swap).clone(),
            live: is_live_swap_use(&record, swap_use),
            spoke_pool,
        })
    }

    /// Admit the Public source and resolve its signer without signing or broadcasting.
    /// Call before destination setup so both signing permissions are obtained before the
    /// swap's first side effect. Retain the authorized source for its approvals, deposit or order.
    pub async fn authorize_public_swap_source(
        &self,
        origin: &EffectiveChainConfig,
        source: PublicSwapSource<'_>,
    ) -> Result<AuthorizedPublicSwapSource> {
        self.ensure_active()?;
        let signer = Box::pin(admitted_public_signer_authorized(
            &self.vault,
            &self.view,
            source.authorization,
            source.public_account_uuid,
            source.trezor_app_passphrase,
            source.trezor_pin_matrix_provider,
            None,
            origin.chain_id,
        ))
        .await?;
        Ok(AuthorizedPublicSwapSource { signer })
    }

    /// The Public account's allowance of `token` to `spender` on the origin chain.
    async fn public_swap_allowance(
        &self,
        origin: &EffectiveChainConfig,
        source: Address,
        token: Address,
        spender: Address,
    ) -> Result<U256> {
        self.while_active(query_erc20_allowance(
            &origin.rpc_route,
            &self.http,
            PublicAssetId::Erc20(token),
            source,
            spender,
        ))
        .await
        .wrap_err("read the Public account's allowance for the swap")
    }

    /// Send one transaction of the swap from its Public account. It is recorded with the
    /// origin chain's head before it is broadcast, and its inclusion once the step observed
    /// it, whether it succeeded or reverted. A stopped swap sends no approval and no deposit.
    /// Its withdrawal and its order's invalidation are still sent.
    pub(super) async fn submit_public_swap_transaction(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        kind: PublicSwapTransactionKind,
        step: PublicActionProgressStep,
        transaction: TransactionRequest,
        gas_fee: PublicActionGasFeeSelection,
        progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
    ) -> Result<PublicSwapStepOutcome> {
        let submitted_from_block = self.while_active(origin_head(origin, &self.http)).await?;
        let ends_swap = matches!(
            kind,
            PublicSwapTransactionKind::Withdrawal | PublicSwapTransactionKind::Invalidation
        );
        let mut handoff = |hash, transaction: &TransactionRequest| {
            if !ends_swap {
                self.require_live_swap_use(operation, Some(swap_use))?;
            }
            self.store.record_public_swap_transaction(
                operation,
                swap_use,
                PublicSwapTransaction {
                    kind,
                    transaction: transaction.clone(),
                    hash,
                    inclusion: None,
                    submitted_from_block: Some(submitted_from_block),
                    deposit_scan_from_block: None,
                },
            )?;
            self.notify_change();
            Ok(())
        };
        let outcome = self
            .while_active(Box::pin(submit_public_swap_step(
                step,
                transaction,
                signer,
                origin,
                gas_fee,
                &self.http,
                &mut handoff,
                progress,
            )))
            .await?;
        let hash: B256 = outcome.receipt.tx_hash.parse()?;
        // The step reports the block's number. Its hash is read by that number, and the
        // hand-off's read checks the transaction against the final block again.
        let block = origin_block(origin, &self.http, outcome.receipt.block_number).await?;
        self.store.record_public_swap_inclusion(
            operation,
            swap_use,
            hash,
            PublicSwapInclusion {
                observation: SwapObservation {
                    block,
                    transaction_hash: Some(hash),
                },
                succeeded: outcome.receipt.status,
                finalized: false,
            },
        )?;
        self.notify_change();
        Ok(outcome)
    }
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

/// The origin chain's head block number, from the first of its endpoints that answers.
async fn origin_head(origin: &EffectiveChainConfig, http: &HttpContext) -> Result<u64> {
    let pool = query_rpc_pool_with_http_client(origin.rpc_route.endpoint_urls(), http);
    for endpoint in pool.available_providers() {
        if let Ok(head) = endpoint.provider.get_block_number().await {
            return Ok(head);
        }
    }
    Err(eyre!("the network the swap pays on is unavailable"))
}

/// The identity of the origin chain's block `number`, from the first of its endpoints that
/// serves it.
async fn origin_block(
    origin: &EffectiveChainConfig,
    http: &HttpContext,
    number: u64,
) -> Result<BlockNumHash> {
    let pool = query_rpc_pool_with_http_client(origin.rpc_route.endpoint_urls(), http);
    for endpoint in pool.available_providers() {
        if let Ok(Some(block)) = endpoint.provider.get_block_by_number(number.into()).await {
            let identity = block.header.num_hash();
            if identity.number == number {
                return Ok(identity);
            }
        }
    }
    Err(eyre!(
        "the block that includes the Public account's transaction is unavailable"
    ))
}

/// Whether `proxy` has code on the chain `pool` reads, from the first of its endpoints that
/// answers.
pub(super) async fn proxy_deployed(pool: &QueryRpcPool, proxy: Address) -> Result<bool> {
    for endpoint in pool.available_providers() {
        if let Ok(code) = endpoint.provider.get_code_at(proxy).await {
            return Ok(!code.is_empty());
        }
    }
    Err(eyre!("the network the swap pays on is unavailable"))
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
}

/// Resume each handed-off deposit independently. Inspect at most eight finalized blocks per
/// poll, matching transaction hashes locally before fetching checked whole-block receipts.
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
        if let Some(inclusion) = deposit.inclusion
            && inclusion.observation.block.number <= finalized_head
            && remaining > 0
        {
            remaining -= 1;
            let (verified, handoff) = read_deposit_block(
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
                });
                if handoff.is_some() {
                    read.handoff = handoff;
                    return Ok(read);
                }
                continue;
            }
        }
        let Some(mut number) = deposit
            .deposit_scan_from_block
            .or(deposit.submitted_from_block)
        else {
            continue;
        };
        let mut update = DepositUpdate {
            hash: deposit.hash,
            inclusion: None,
            next_block: None,
        };
        while remaining > 0 && number <= finalized_head {
            remaining -= 1;
            let (inclusion, handoff) =
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
            let Some(next) = number.checked_add(1) else {
                break;
            };
            number = next;
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

/// Verify both successful and failed receipts. A successful deposit must carry the approved
/// terms; a missing or inconsistent receipt never advances the durable scan cursor.
async fn read_deposit_block(
    provider: &DynProvider,
    number: u64,
    hash: B256,
    expected: &ExpectedHandoff<'_>,
) -> Result<(Option<PublicSwapInclusion>, Option<ObservedHandoff>)> {
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
    Ok((inclusion, handoff))
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
    fn approvals_reset_a_short_nonzero_allowance_first() {
        let amount = U256::from(200);
        assert!(public_swap_approvals(amount, amount).is_empty());
        assert!(public_swap_approvals(U256::MAX, amount).is_empty());
        assert_eq!(public_swap_approvals(U256::ZERO, amount), [amount]);
        assert_eq!(
            public_swap_approvals(U256::from(100), amount),
            [U256::ZERO, amount]
        );
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
}
