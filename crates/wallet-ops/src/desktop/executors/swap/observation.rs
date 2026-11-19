//! Account-specific reconciliation for explicit swap actions.
//! Routine background confirmation uses `settlement` and only whole-block requests.
//!
//! Every observation comes from chain data at or below the executor's reconciled
//! confirmed block, never from the orderbook's order status. Logs locate events
//! inside the requested page. State read at the confirmed block decides what can
//! no longer happen: spent nullifiers, the execution nonce, the order's filled
//! amount, and, for Reshield and Across delivery, the executor's buy-token balance. External
//! delivery is established by the trade alone. State also decides whether the page
//! can hold anything new, and its logs are read only when it can. Each recorded
//! observation keeps its block and is dropped once that block is no longer canonical.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use alloy::eips::{BlockId, BlockNumHash};
use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::providers::{DynProvider, Provider as _};
use alloy::rpc::types::{Filter, Log, TransactionRequest};
use alloy::sol_types::{SolCall, SolEvent, SolValue as _};
use broadcaster_core::contracts::cow::OrderUid;
use broadcaster_core::contracts::railgun::{
    Nullified, RelayAdapt7702, Shield, ShieldRequest, Transact, Transaction,
};
use eyre::{Result, eyre};
use sync_service::WalletCurrentSnapshot;
use tracing::Instrument as _;

use super::super::ExecutorReconciliationReport;
use super::super::attribution::{
    ExecutorAttributionEvidence, InvalidatedSwapOrders, attributed_action,
};
use crate::ExecutorOwner;
use crate::OperationHttpClient;
use crate::desktop::executor_discovery::execution_nonce_at;
use crate::desktop::executor_observation::{
    ObservationEndpoints, expected_shields, private_effects_present, trace_step,
};
use crate::public_wallet::PublicErc20;
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    AcrossOrderTerms, BridgeDelivery, BridgeOrderTerms, ExecutorOperationId,
    ExecutorPayloadPurpose, ExecutorRecord, IssuedExecutorPayload, SwapBridgeOutcome, SwapDelivery,
    SwapFunding, SwapObservation, SwapOperationRecord, SwapOrderObservations, SwapOrderRecord,
    SwapPreHookDeath, SwapPreHookDeathCause, SwapShieldObservation, SwapTradeAmounts,
};

mod derivation;
mod facts;
use derivation::derive_swap_observations;
use facts::read_swap_facts;

/// The most blocks one observation page, and so one of its log queries, spans.
pub(super) const MAX_OBSERVATION_BLOCKS: u64 = 64;

// The pinned shared bindings have no settlement event or reads, and no getter for
// Railgun's public nullifier map. These match the deployed contracts.
alloy::sol! {
    interface SwapSettlement {
        event Trade(address indexed owner, address sellToken, address buyToken, uint256 sellAmount, uint256 buyAmount, uint256 feeAmount, bytes orderUid);
        function filledAmount(bytes orderUid) external view returns (uint256);
    }

    interface SwapRailgun {
        function nullifiers(uint256 treeNumber, bytes32 nullifier) external view returns (bool);
    }
}

/// Where one order stands, derived only from its recorded canonical observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapOrderState {
    /// Nothing canonical is observed yet. The order may still fill.
    Open,
    /// The pre-hook ran, or a pre-hook sharing its proof and nonce funded the account, and no
    /// trade is observed. Retries stay blocked. Once `expired`, the order can no longer fill
    /// and the sell token can be recovered.
    PreHookOnly { expired: bool },
    /// Traded, and neither the reshield nor an Across deposit is established yet. External
    /// delivery never stays here, and NEAR Intents only until its settlement receipt is read.
    Traded,
    /// A Bridge order handed off on this chain, without an outcome on the destination chain.
    Bridging,
    /// Traded and delivered: reshielded, paid to the External receiver, or delivered by the
    /// bridge on the destination chain.
    Done,
    /// Traded, and the executor still held buy tokens at a finalized block. Recovery of the
    /// buy token is offered. Reshield delivery and an Across post-hook that didn't run reach this.
    NotDelivered,
    /// The bridge expired or refunded the deposit, which returns the bought token to the
    /// executor. Recovery of it is offered once an explicit check finds it there, for Across
    /// after that check also verified the refund on this chain.
    Refunding,
    /// The bridge provider reported a failed or incomplete deposit. The wallet can't recover
    /// it; an explicit status check may still resolve it.
    NeedsAttention,
    /// The fill completed without the shield. The destination stealth account holds the token;
    /// recovery is offered on the destination chain.
    HeldOnDestination,
    /// The pre-hook can never run. Its inputs are released and a retry may start.
    AttemptEnded(SwapPreHookDeathCause),
    /// Another attempt of the same swap use, signed at the same pre-hook nonce, traded with
    /// the funding this order shared. This order can't fill and holds nothing to recover.
    Superseded,
}

/// Completion needs both the trade and delivery, and for a Bridge order the destination
/// outcome after its hand-off. Without a trade, `swap`'s verified funding at the order's use
/// and pre-hook nonce counts like its own executed pre-hook, until a sibling attempt at that
/// nonce trades and supersedes the order. Observations are the last recorded ones; current
/// decisions need a reconciled record.
#[must_use]
pub fn swap_order_state(swap: &SwapOperationRecord, order: &SwapOrderRecord) -> SwapOrderState {
    order_state(
        order,
        swap.funding(order).is_some(),
        swap.traded_sibling(order).is_some(),
    )
}

/// [`swap_order_state`] with `funded` saying whether the order shares a verified funding and
/// `superseded` whether a sibling attempt at its pre-hook nonce traded.
fn order_state(order: &SwapOrderRecord, funded: bool, superseded: bool) -> SwapOrderState {
    let observed = order.observations();
    if observed.traded.is_some() {
        return match (observed.delivered, order.delivery()) {
            (None, _) if observed.undelivered.is_some() => SwapOrderState::NotDelivered,
            (None, _) => SwapOrderState::Traded,
            // A Bridge order's delivery on this chain is its hand-off.
            (Some(_), SwapDelivery::Bridge(_) | SwapDelivery::NonEvmBridge(_)) => {
                bridge_state(observed.bridge_outcome.as_ref())
            }
            (Some(_), SwapDelivery::Reshield | SwapDelivery::External { .. }) => {
                SwapOrderState::Done
            }
        };
    }
    // A recorded death keeps its cause.
    if superseded && observed.pre_hook_dead.is_none() {
        return SwapOrderState::Superseded;
    }
    if observed.pre_hook_executed.is_some() || funded {
        return SwapOrderState::PreHookOnly {
            expired: observed.expired.is_some(),
        };
    }
    observed
        .pre_hook_dead
        .map_or(SwapOrderState::Open, |death| {
            SwapOrderState::AttemptEnded(death.cause)
        })
}

/// Where a handed-off Bridge order stands, from its outcome on the destination chain.
const fn bridge_state(outcome: Option<&SwapBridgeOutcome>) -> SwapOrderState {
    match outcome {
        None => SwapOrderState::Bridging,
        Some(outcome) if outcome.is_delivered() => SwapOrderState::Done,
        Some(SwapBridgeOutcome::Refunding) => SwapOrderState::Refunding,
        Some(SwapBridgeOutcome::HeldOnDestination { .. }) => SwapOrderState::HeldOnDestination,
        Some(_) => SwapOrderState::NeedsAttention,
    }
}

/// Where `order` stands as an attempt of its swap use. Equal to [`swap_order_state`] except
/// when the use's trade and delivery sit on different attempts: then the traded attempt and
/// the attempt holding the hand-off both report the bridge's state, from the hand-off
/// attempt's outcome.
#[must_use]
pub fn swap_attempt_state(record: &ExecutorRecord, order: &SwapOrderRecord) -> SwapOrderState {
    let observed = order.observations();
    let paired = record.swap().and_then(|swap| {
        if observed.traded.is_some() {
            if observed.delivered.is_some() {
                return None;
            }
            swap.delivery_order(order)
        } else {
            observed.bridge_handoff?;
            swap.trade_order(order).map(|_| order)
        }
    });
    paired.map_or_else(
        || {
            let swap = record.swap();
            order_state(
                order,
                swap.is_some_and(|swap| swap.funding(order).is_some()),
                swap.is_some_and(|swap| swap.traded_sibling(order).is_some()),
            )
        },
        |delivery| bridge_state(delivery.observations().bridge_outcome.as_ref()),
    )
}

impl ExecutorOwner {
    /// Read the swap executor's account at the confirmed tip, then update every order's
    /// observations from that block, locating the swap's own events in `range`. The
    /// orderbook is not consulted. If the swap read fails, the executor needs
    /// reconciliation again before any recorded outcome is relied on.
    pub async fn observe_swap(
        &self,
        operation: ExecutorOperationId,
        range: Range<u64>,
        client: &OperationHttpClient,
    ) -> Result<ExecutorReconciliationReport> {
        let sync = self.synced_snapshot();
        Box::pin(self.observe_swap_synced(operation, range, sync, client)).await
    }

    /// Private sync's current snapshot. `None` before sync is attached and while it has none.
    pub(in crate::desktop::executors) fn synced_snapshot(
        &self,
    ) -> Option<Arc<WalletCurrentSnapshot>> {
        let wallet = self
            .synced_observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|(wallet, _)| wallet.clone())?;
        wallet.current_snapshot()
    }

    /// [`Self::observe_swap`] with `sync` as private sync's snapshot, which attributes a
    /// pre-hook's consumed nonce. It is read only and starts no chain read.
    pub(crate) async fn observe_swap_synced(
        &self,
        operation: ExecutorOperationId,
        range: Range<u64>,
        sync: Option<Arc<WalletCurrentSnapshot>>,
        client: &OperationHttpClient,
    ) -> Result<ExecutorReconciliationReport> {
        if self
            .swap_record(operation)?
            .is_none_or(|record| record.swap().is_none())
        {
            return Err(eyre!("swap order is unavailable"));
        }
        let report = trace_step("swap_account", self.reconcile_account(operation, client)).await?;
        let observed = self
            .read_swap_observations(report.record(), range, sync.as_deref(), client)
            .await;
        let _guard = self.lock_activity().await;
        self.require_record_unchanged(report.record())?;
        self.apply_swap_observations(report, observed)
    }

    async fn read_swap_observations(
        &self,
        record: &ExecutorRecord,
        range: Range<u64>,
        sync: Option<&WalletCurrentSnapshot>,
        client: &OperationHttpClient,
    ) -> Result<SwapObserved> {
        let mut chain = self
            .chain_for_delegate(record.delegate())
            .ok_or_else(|| eyre!("chain does not support Railgun"))?;
        chain.enabled = true;
        let endpoints = self.observation_endpoints(client)?;
        trace_step(
            "swap_orders",
            self.while_active(Box::pin(observe_swap_orders(
                &endpoints, &chain, record, range, sync,
            ))),
        )
        .await
    }

    fn apply_swap_observations(
        &self,
        report: ExecutorReconciliationReport,
        observed: Result<SwapObserved>,
    ) -> Result<ExecutorReconciliationReport> {
        let ExecutorReconciliationReport { mut record, code } = report;
        let operation = record.operation();
        let observed = match observed {
            Ok(observed) => observed,
            Err(error) => {
                self.store.invalidate_observation(operation)?;
                return Err(error);
            }
        };
        // Fundings are written first: an interrupted update then leaves an order funded
        // without naming its hook, which every reader handles, instead of an executed
        // pre-hook whose sibling attempt reads as open.
        if record
            .swap()
            .is_some_and(|swap| swap.fundings() != observed.fundings)
        {
            record = self
                .store
                .record_swap_fundings(operation, observed.fundings)?;
        }
        for (uid, observations) in observed.orders {
            let changed = record
                .swap()
                .and_then(|swap| swap.orders().iter().find(|order| order.uid() == uid))
                .is_some_and(|order| order.observations() != observations);
            if changed {
                record = self
                    .store
                    .record_swap_observations(operation, uid, observations)?;
            }
        }
        self.notify_change();
        Ok(ExecutorReconciliationReport { record, code })
    }

    /// Whether every recorded order of `record` is closed at its freshly reconciled
    /// confirmed block. Spent hook nonces don't close a signed order, and the pre-hook's
    /// sell-token approval to the vault relayer stays. Recorded observations aren't
    /// revalidated after a reorg, so each order is read again.
    pub(crate) async fn swap_orders_closed(
        &self,
        chain: &EffectiveChainConfig,
        record: &ExecutorRecord,
        client: &OperationHttpClient,
    ) -> Result<bool> {
        let endpoints = self.observation_endpoints(client)?;
        for provider in endpoints.providers().await {
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = provider.index);
            match trace_step(
                "swap_orders_closed_rpc",
                read_swap_orders_closed(&provider.provider, chain, record),
            )
            .instrument(span)
            .await
            {
                Ok(closed) => {
                    endpoints.succeeded(&provider);
                    return Ok(closed);
                }
                Err(error) => endpoints.failed(&provider, &error),
            }
        }
        Err(eyre!("swap order state is unavailable"))
    }
}

/// An order is closed at the confirmed block once it filled in full, the settlement
/// invalidated it (`filledAmount` is the maximum), or the block is past its `validTo`.
async fn read_swap_orders_closed(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    record: &ExecutorRecord,
) -> Result<bool> {
    let (Some(observed), Some(swap)) = (record.nonce_observation(), record.swap()) else {
        return Err(eyre!("swap executor is not reconciled"));
    };
    let confirmed = observed.block();
    let settlement = chain
        .swap_profile()
        .ok_or_else(|| eyre!("private swaps are unavailable on this chain"))?
        .settlement();
    let mut headers = Headers {
        provider,
        confirmed: confirmed.number,
        cache: BTreeMap::new(),
    };
    let tip = headers.at(confirmed.number).await?;
    if tip.hash != confirmed.hash {
        return Err(eyre!("swap observation block is no longer canonical"));
    }
    let at_confirmed = BlockId::hash_canonical(confirmed.hash);
    for order in swap.orders() {
        if tip.timestamp > u64::from(order.valid_to()) {
            continue;
        }
        let filled = filled_amount(provider, settlement, order.uid(), at_confirmed).await?;
        if filled != order.bounds().sell_amount && filled != U256::MAX {
            return Ok(false);
        }
    }
    // A block fetched before a reorg may still be served afterwards.
    if trace_step("swap_canonical_recheck", async {
        provider.get_block_by_number(confirmed.number.into()).await
    })
    .await?
    .is_none_or(|current| current.header.hash != confirmed.hash)
    {
        return Err(eyre!("swap chain changed during observation"));
    }
    Ok(true)
}

async fn observe_swap_orders(
    endpoints: &ObservationEndpoints,
    chain: &EffectiveChainConfig,
    record: &ExecutorRecord,
    range: Range<u64>,
    sync: Option<&WalletCurrentSnapshot>,
) -> Result<SwapObserved> {
    let railgun = chain.require_railgun()?.deployment.contract;
    for provider in endpoints.providers().await {
        let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = provider.index);
        match trace_step(
            "swap_facts_rpc",
            read_swap_facts(&provider.provider, chain, record, range.clone(), sync),
        )
        .instrument(span)
        .await
        {
            Ok(facts) => {
                endpoints.succeeded(&provider);
                let orders = derive_swap_observations(record, railgun, sync, &facts);
                return Ok(SwapObserved {
                    orders,
                    fundings: facts.fundings,
                });
            }
            Err(error) => endpoints.failed(&provider, &error),
        }
    }
    Err(eyre!(
        "swap observations are unavailable; the swap's recorded state is unchanged"
    ))
}

/// One read's result: every order's observations and the swap's verified fundings.
struct SwapObserved {
    /// In the record's order.
    orders: Vec<(OrderUid, SwapOrderObservations)>,
    fundings: Vec<SwapFunding>,
}

/// What one read established, before it is combined with the order's recorded
/// observations that are still canonical.
#[derive(Default)]
struct OrderFacts {
    retained: SwapOrderObservations,
    /// This order's pre-hook ran. Left unset when another attempt's pre-hook shares the
    /// evidence, which then only shows a funding.
    executed: Option<SwapObservation>,
    traded: Option<SwapObservation>,
    /// The executed amounts of the trade in `traded`.
    trade_amounts: Option<SwapTradeAmounts>,
    /// The latest shield in the page made by this order's post-hook.
    shielded: Option<SwapShieldObservation>,
    /// Whether every pre-hook nullifier was spent at the confirmed block, when read.
    nullifiers_spent: Option<bool>,
    /// The settlement's `filledAmount` at the confirmed block, when read.
    filled: Option<U256>,
    /// This order's buy-token balance at the confirmed block. Read only for orders that pay the
    /// executor: Reshield and Across delivery.
    buy_balance: Option<U256>,
    /// This order's trade is delivered by another attempt of its swap use, whose retained
    /// hand-off is in the trade's receipt.
    delivered_by_use: bool,
    /// The funding at this order's swap use and pre-hook nonce sits in a block past the
    /// order's `validTo`. The deadline guard reverts there, so another attempt's pre-hook ran.
    funding_after_valid_to: bool,
}

struct SwapFacts {
    settlement: Address,
    confirmed: BlockNumHash,
    timestamp: u64,
    nonce: U256,
    /// In the record's order.
    orders: Vec<OrderFacts>,
    /// The recorded fundings that are still canonical and the ones this read found, at most
    /// one per swap use and pre-hook nonce.
    fundings: Vec<SwapFunding>,
}

/// [`SwapOperationRecord::funding`] over `fundings` and the order's retained observations,
/// so a reorged funding or death counts for nothing.
fn shares_funding(
    fundings: &[SwapFunding],
    order: &SwapOrderRecord,
    retained: &SwapOrderObservations,
) -> bool {
    retained.pre_hook_dead.is_none()
        && order.use_id().is_some_and(|use_id| {
            fundings.iter().any(|funding| {
                funding.use_id == use_id && funding.pre_hook_nonce == order.pre_hook().nonce()
            })
        })
}

#[derive(Clone, Copy)]
struct Header {
    hash: B256,
    parent: B256,
    timestamp: u64,
}

struct Headers<'a> {
    provider: &'a DynProvider,
    confirmed: u64,
    cache: BTreeMap<u64, Header>,
}

impl Headers<'_> {
    async fn at(&mut self, number: u64) -> Result<Header> {
        if let Some(header) = self.cache.get(&number) {
            return Ok(*header);
        }
        let block = trace_step("swap_header", async {
            self.provider.get_block_by_number(number.into()).await
        })
        .await?
        .ok_or_else(|| eyre!("swap observation block is unavailable"))?;
        if block.header.number != number {
            return Err(eyre!("swap observation block does not match its height"));
        }
        let header = Header {
            hash: block.header.hash,
            parent: block.header.parent_hash,
            timestamp: block.header.timestamp,
        };
        self.cache.insert(number, header);
        Ok(header)
    }

    async fn canonical(&mut self, block: BlockNumHash) -> Result<bool> {
        Ok(block.number <= self.confirmed && self.at(block.number).await?.hash == block.hash)
    }

    async fn keep(
        &mut self,
        observation: Option<SwapObservation>,
    ) -> Result<Option<SwapObservation>> {
        let Some(observed) = observation else {
            return Ok(None);
        };
        Ok(self.canonical(observed.block).await?.then_some(observed))
    }

    /// Keep a recorded `value` while its observation, which `observation` reads, is canonical.
    async fn keep_with<T>(
        &mut self,
        value: Option<T>,
        observation: impl FnOnce(&T) -> SwapObservation,
    ) -> Result<Option<T>> {
        let Some(value) = value else {
            return Ok(None);
        };
        Ok(self.keep(Some(observation(&value))).await?.map(|_| value))
    }

    /// Evidence found in this read must be canonical, or the chain moved under it.
    async fn evidence(
        &mut self,
        block: BlockNumHash,
        transaction: B256,
    ) -> Result<SwapObservation> {
        if !self.canonical(block).await? {
            return Err(eyre!("swap chain changed during observation"));
        }
        Ok(SwapObservation {
            block,
            transaction_hash: Some(transaction),
        })
    }
}

pub(super) async fn filled_amount(
    provider: &DynProvider,
    settlement: Address,
    uid: OrderUid,
    block: BlockId,
) -> Result<U256> {
    call_at(
        provider,
        settlement,
        SwapSettlement::filledAmountCall {
            orderUid: uid.0.to_vec().into(),
        },
        block,
    )
    .await
}

pub(super) fn issued(record: &ExecutorRecord, hash: B256) -> Result<&IssuedExecutorPayload> {
    record
        .issued()
        .iter()
        .find(|payload| payload.hash() == hash)
        .ok_or_else(|| eyre!("swap hook payload is not recorded"))
}

/// A post-hook shields the full balance, so its request's value is zero and the
/// event's value is whatever the executor held. The note key and ciphertext are
/// the post-hook's own. The credited amount comes with the event's fee for the same
/// commitment, or `None` when the event doesn't carry one.
pub(super) fn shielded_amount(
    railgun: Address,
    log: &Log,
    requests: &[ShieldRequest],
) -> Option<(U256, Option<U256>)> {
    if log.address() != railgun || log.removed {
        return None;
    }
    let Ok(event) = log.log_decode::<Shield>() else {
        return None;
    };
    let event = event.inner.data;
    event
        .commitments
        .iter()
        .zip(&event.shieldCiphertext)
        .enumerate()
        .filter(|(_, (preimage, ciphertext))| {
            !preimage.value.is_zero()
                && requests.iter().any(|request| {
                    preimage.npk == request.preimage.npk
                        && preimage.token.abi_encode() == request.preimage.token.abi_encode()
                        && ciphertext.abi_encode() == request.ciphertext.abi_encode()
                })
        })
        .map(|(index, (preimage, _))| (U256::from(preimage.value), event.fees.get(index).copied()))
        .max_by_key(|(amount, _)| *amount)
}

pub(super) async fn call_at<C: SolCall>(
    provider: &DynProvider,
    to: Address,
    call: C,
    block: BlockId,
) -> Result<C::Return> {
    let span = tracing::debug_span!(target: "executor_observation", "contract_call", method = C::SIGNATURE);
    let output = trace_step("swap_contract_call", async {
        provider
            .call(
                TransactionRequest::default()
                    .to(to)
                    .input(call.abi_encode().into()),
            )
            .block(block)
            .await
    })
    .instrument(span)
    .await?;
    Ok(C::abi_decode_returns(&output)?)
}
