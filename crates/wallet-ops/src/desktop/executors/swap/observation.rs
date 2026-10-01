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

use alloy::eips::{BlockId, BlockNumHash};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{DynProvider, Provider as _};
use alloy::rpc::types::{Filter, Log, TransactionRequest};
use alloy::sol_types::{SolCall, SolEvent, SolValue as _};
use broadcaster_core::contracts::cow::OrderUid;
use broadcaster_core::contracts::railgun::{
    Nullified, RelayAdapt7702, Shield, ShieldRequest, Transact, Transaction,
};
use eyre::{Result, eyre};
use tracing::Instrument as _;

use super::super::ExecutorReconciliationReport;
use crate::ExecutorOwner;
use crate::desktop::executor_discovery::execution_nonce_at;
use crate::desktop::executor_observation::{
    ObservationEndpoints, expected_shields, private_effects_present, trace_step,
};
use crate::public_wallet::PublicErc20;
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    ExecutorOperationId, ExecutorPayloadPurpose, ExecutorPayloadStatus, ExecutorRecord,
    IssuedExecutorPayload, SwapBridgeHandoff, SwapBridgeOutcome, SwapDelivery, SwapObservation,
    SwapOrderObservations, SwapOrderRecord, SwapPreHookDeath, SwapPreHookDeathCause,
    SwapShieldObservation, SwapTradeAmounts,
};

const MAX_OBSERVATION_BLOCKS: u64 = 64;

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
    /// The pre-hook ran and no trade is observed. Retries stay blocked. Once
    /// `expired`, the order can no longer fill and the sell token can be recovered.
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
    /// The pre-hook can never run. Its inputs are released and a retry may start.
    AttemptEnded(SwapPreHookDeathCause),
}

/// Completion needs both the trade and delivery, and for a Bridge order the destination
/// outcome after its hand-off. Observations are the last recorded ones; current decisions
/// need a reconciled record.
#[must_use]
pub fn swap_order_state(order: &SwapOrderRecord) -> SwapOrderState {
    let observed = order.observations();
    if observed.traded.is_some() {
        return match (observed.delivered, order.delivery()) {
            (None, _) if observed.undelivered.is_some() => SwapOrderState::NotDelivered,
            (None, _) => SwapOrderState::Traded,
            // A Bridge order's delivery on this chain is its hand-off.
            (Some(_), SwapDelivery::Bridge(_)) => match observed.bridge_outcome {
                None => SwapOrderState::Bridging,
                Some(outcome) if outcome.is_delivered() => SwapOrderState::Done,
                Some(SwapBridgeOutcome::Refunding) => SwapOrderState::Refunding,
                Some(_) => SwapOrderState::NeedsAttention,
            },
            (Some(_), SwapDelivery::Reshield | SwapDelivery::External { .. }) => {
                SwapOrderState::Done
            }
        };
    }
    if observed.pre_hook_executed.is_some() {
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

impl ExecutorOwner {
    /// Reconcile the swap executor over `range`, then update every order's
    /// observations from the same confirmed block. The orderbook is not consulted.
    /// If the swap read fails, the executor needs reconciliation again before any
    /// recorded outcome is relied on.
    pub async fn observe_swap(
        &self,
        operation: ExecutorOperationId,
        range: Range<u64>,
    ) -> Result<ExecutorReconciliationReport> {
        if self
            .swap_record(operation)?
            .is_none_or(|record| record.swap().is_none())
        {
            return Err(eyre!("swap order is unavailable"));
        }
        let report = trace_step(
            "swap_history",
            self.reconcile_history(operation, range.clone()),
        )
        .await?;
        let observed = self.read_swap_observations(report.record(), range).await;
        let _guard = self.lock_activity().await;
        self.require_record_unchanged(report.record())?;
        self.apply_swap_observations(report, observed)
    }

    async fn read_swap_observations(
        &self,
        record: &ExecutorRecord,
        range: Range<u64>,
    ) -> Result<Vec<(OrderUid, SwapOrderObservations)>> {
        let mut chain = self.chain.clone();
        chain
            .railgun
            .as_mut()
            .ok_or_else(|| eyre!("chain does not support Railgun"))?
            .deployment
            .relay_adapt_7702_contract = record.delegate();
        chain.enabled = true;
        trace_step(
            "swap_orders",
            self.while_active(Box::pin(observe_swap_orders(
                &self.endpoints,
                &chain,
                record,
                range,
            ))),
        )
        .await
    }

    fn apply_swap_observations(
        &self,
        report: ExecutorReconciliationReport,
        observed: Result<Vec<(OrderUid, SwapOrderObservations)>>,
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
        for (uid, observations) in observed {
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
    ) -> Result<bool> {
        for provider in self.endpoints.providers().await {
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = provider.index);
            match trace_step(
                "swap_orders_closed_rpc",
                read_swap_orders_closed(&provider.provider, chain, record),
            )
            .instrument(span)
            .await
            {
                Ok(closed) => {
                    self.endpoints.succeeded(&provider);
                    return Ok(closed);
                }
                Err(error) => self.endpoints.failed(&provider, &error),
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
) -> Result<Vec<(OrderUid, SwapOrderObservations)>> {
    let railgun = chain.require_railgun()?.deployment.contract;
    for provider in endpoints.providers().await {
        let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = provider.index);
        match trace_step(
            "swap_facts_rpc",
            read_swap_facts(&provider.provider, chain, record, range.clone()),
        )
        .instrument(span)
        .await
        {
            Ok(facts) => {
                endpoints.succeeded(&provider);
                return Ok(derive_swap_observations(record, railgun, &facts));
            }
            Err(error) => endpoints.failed(&provider, &error),
        }
    }
    Err(eyre!(
        "swap observations are unavailable; the swap's recorded state is unchanged"
    ))
}

/// What one read established, before it is combined with the order's recorded
/// observations that are still canonical.
#[derive(Default)]
struct OrderFacts {
    retained: SwapOrderObservations,
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
}

struct SwapFacts {
    confirmed: BlockNumHash,
    timestamp: u64,
    nonce: U256,
    /// In the record's order.
    orders: Vec<OrderFacts>,
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

async fn read_swap_facts(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    record: &ExecutorRecord,
    range: Range<u64>,
) -> Result<SwapFacts> {
    let (Some(observed), Some(executor), Some(swap)) =
        (record.nonce_observation(), record.address(), record.swap())
    else {
        return Err(eyre!("swap executor is not reconciled"));
    };
    let confirmed = observed.block();
    if range.is_empty()
        || range.end - range.start > MAX_OBSERVATION_BLOCKS
        || range.end - 1 > confirmed.number
    {
        return Err(eyre!(
            "swap observation requires between 1 and 64 confirmed blocks"
        ));
    }
    let settlement = chain
        .swap_profile()
        .ok_or_else(|| eyre!("private swaps are unavailable on this chain"))?
        .settlement();
    let railgun = chain.require_railgun()?.deployment.contract;
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

    let mut orders = Vec::with_capacity(swap.orders().len());
    for order in swap.orders() {
        let recorded = order.observations();
        let pre_hook_dead = match recorded.pre_hook_dead {
            Some(death) => headers.keep(Some(death.observation)).await?.map(|_| death),
            None => None,
        };
        let traded = headers.keep(recorded.traded).await?;
        orders.push(OrderFacts {
            retained: SwapOrderObservations {
                pre_hook_executed: headers.keep(recorded.pre_hook_executed).await?,
                traded,
                trade_amounts: recorded.trade_amounts.filter(|_| traded.is_some()),
                delivered: headers.keep(recorded.delivered).await?,
                shielded: if let Some(shield) = recorded.shielded {
                    headers
                        .keep(Some(shield.observation))
                        .await?
                        .map(|observation| SwapShieldObservation {
                            observation,
                            ..shield
                        })
                } else {
                    None
                },
                settlement_credit: if let Some(credit) = recorded.settlement_credit {
                    headers
                        .keep(Some(credit.observation))
                        .await?
                        .map(|observation| SwapShieldObservation {
                            observation,
                            ..credit
                        })
                } else {
                    None
                },
                pre_hook_dead,
                undelivered: headers.keep(recorded.undelivered).await?,
                expired: headers.keep(recorded.expired).await?,
                bridge_handoff: if let Some(handoff) = recorded.bridge_handoff {
                    headers
                        .keep(Some(handoff.observation))
                        .await?
                        .map(|observation| SwapBridgeHandoff {
                            observation,
                            ..handoff
                        })
                } else {
                    None
                },
                post_hook_deposit: headers.keep(recorded.post_hook_deposit).await?,
                // The destination chain's outcome isn't evidence on this chain.
                bridge_outcome: recorded.bridge_outcome,
                bridge_refund: headers.keep(recorded.bridge_refund).await?,
            },
            ..OrderFacts::default()
        });
    }

    // State at the confirmed block decides first whether the page's logs can hold
    // anything new. When they can't, the page is covered without reading them.
    let page_needed = page_may_hold_hook_evidence(
        record,
        swap.orders(),
        &orders,
        observed.nonce(),
        tip.timestamp,
    ) || any_order_filled(
        provider,
        settlement,
        swap.orders(),
        &mut orders,
        at_confirmed,
        &mut headers,
        range.start,
    )
    .await?;
    let page = |filter: Filter| filter.from_block(range.start).to_block(range.end - 1);
    let trades = if page_needed {
        trace_step("swap_trade_logs", async {
            provider
                .get_logs(&page(
                    Filter::new()
                        .address(settlement)
                        .event_signature(SwapSettlement::Trade::SIGNATURE_HASH)
                        .topic1(executor),
                ))
                .await
        })
        .await?
    } else {
        Vec::new()
    };
    // Railgun events carry no indexed fields, so the page is read whole.
    let mut railgun_transactions = BTreeMap::<(u64, B256), (BlockNumHash, Vec<Log>)>::new();
    let railgun_logs = if page_needed {
        trace_step("swap_railgun_logs", async {
            provider
                .get_logs(&page(Filter::new().address(railgun).event_signature(vec![
                    Nullified::SIGNATURE_HASH,
                    Transact::SIGNATURE_HASH,
                    Shield::SIGNATURE_HASH,
                ])))
                .await
        })
        .await?
    } else {
        Vec::new()
    };
    for log in railgun_logs {
        let (block, transaction) = located(&log, &range)?;
        railgun_transactions
            .entry((block.number, transaction))
            .or_insert_with(|| (block, Vec::new()))
            .1
            .push(log);
    }

    for (order, facts) in swap.orders().iter().zip(&mut orders) {
        let pre_hook = RelayAdapt7702::executeCall::abi_decode(
            issued(record, order.pre_hook().payload())?
                .context()
                .calldata(),
        )?
        ._transactions;
        // An order without a post-hook has no post-hook shield to find.
        let post_hook_shields = match order.post_hook() {
            Some(post_hook) => expected_shields(
                executor,
                railgun,
                &RelayAdapt7702::multicallCall::abi_decode(
                    issued(record, post_hook.payload())?.context().calldata(),
                )?
                ._calls,
            )?,
            None => Vec::new(),
        };
        // Receipt confirmation established delivery without identifying RPC reads. During
        // explicit account reuse/recovery, establish the post-hook nonce at that known block.
        if facts.retained.shielded.is_none()
            && let Some(credit) = facts.retained.settlement_credit
            && let Some(post_hook) = order.post_hook()
            && nonce_passed_in(
                provider,
                chain,
                executor,
                &mut headers,
                credit.observation.block,
                post_hook.nonce(),
            )
            .await?
        {
            facts.retained.shielded = Some(credit);
        }
        // Likewise, a receipt's Across deposit is the post-hook's once its nonce passed there.
        if facts.retained.post_hook_deposit.is_none()
            && let Some(handoff) = facts.retained.bridge_handoff
            && handoff.deposit_id.is_some()
            && let Some(post_hook) = order.post_hook()
            && nonce_passed_in(
                provider,
                chain,
                executor,
                &mut headers,
                handoff.observation.block,
                post_hook.nonce(),
            )
            .await?
        {
            facts.retained.post_hook_deposit = Some(handoff.observation);
        }
        let nonce = order.pre_hook().nonce();
        let valid_to = u64::from(order.valid_to());

        for ((_, transaction), (block, logs)) in &railgun_transactions {
            if facts.retained.pre_hook_executed.is_none()
                && facts.retained.pre_hook_dead.is_none()
                && facts.executed.is_none()
                && spends_all(&pre_hook)
                && private_effects_present(railgun, &pre_hook, logs)
            {
                let evidence = headers.evidence(*block, *transaction).await?;
                // The deadline guard reverts after `validTo`, and only this order's
                // pre-hook consumes its nonce `k`. A reused proof shares nullifiers
                // with an earlier attempt's pre-hook, which these checks exclude.
                if headers.at(block.number).await?.timestamp <= valid_to
                    && nonce_passed_in(provider, chain, executor, &mut headers, *block, nonce)
                        .await?
                {
                    facts.executed = Some(evidence);
                }
            }
            // Anyone can shield with the post-hook's public request, but only the
            // post-hook itself also moves the nonce past its own in that block.
            // The largest credit is chosen by amount alone; its fee only rides along.
            if let Some(post_hook) = order.post_hook()
                && let Some((private_amount, fee)) = logs
                    .iter()
                    .filter_map(|log| shielded_amount(railgun, log, &post_hook_shields))
                    .max_by_key(|(private_amount, _)| *private_amount)
            {
                let evidence = headers.evidence(*block, *transaction).await?;
                if nonce_passed_in(
                    provider,
                    chain,
                    executor,
                    &mut headers,
                    *block,
                    post_hook.nonce(),
                )
                .await?
                {
                    facts.shielded = Some(SwapShieldObservation {
                        observation: evidence,
                        private_amount,
                        fee,
                    });
                }
            }
        }
        if facts.retained.traded.is_none() {
            for log in &trades {
                let Ok(trade) = log.log_decode::<SwapSettlement::Trade>() else {
                    continue;
                };
                let trade = trade.inner.data;
                if log.address() != settlement
                    || trade.owner != executor
                    || trade.orderUid != order.uid().0[..]
                {
                    continue;
                }
                let (block, transaction) = located(log, &range)?;
                facts.traded = Some(headers.evidence(block, transaction).await?);
                facts.trade_amounts = Some(SwapTradeAmounts {
                    sell_amount: trade.sellAmount,
                    buy_amount: trade.buyAmount,
                    fee_amount: trade.feeAmount,
                    settlement_gas_used: None,
                    settlement_effective_gas_price: None,
                    executed_fee: None,
                    executed_fee_token: None,
                });
                break;
            }
        }
        let executed = facts.retained.pre_hook_executed.or(facts.executed);
        let traded = facts.retained.traded.or(facts.traded);
        if executed.is_none() && facts.retained.pre_hook_dead.is_none() && observed.nonce() > nonce
        {
            facts.nullifiers_spent =
                Some(nullifiers_spent(provider, railgun, &pre_hook, at_confirmed).await?);
        }
        if traded.is_none()
            && facts.retained.expired.is_none()
            && tip.timestamp > valid_to
            && facts.filled.is_none()
        {
            facts.filled =
                Some(filled_amount(provider, settlement, order.uid(), at_confirmed).await?);
        }
        // External and NEAR Intents orders pay elsewhere, so the executor's buy balance proves
        // nothing for them.
        if facts.retained.traded.or(facts.traded).is_some()
            && facts.retained.delivered.is_none()
            && order.delivery().pays_executor()
        {
            facts.buy_balance = Some(
                call_at(
                    provider,
                    swap.order_terms(order).buy_token(),
                    PublicErc20::balanceOfCall { account: executor },
                    at_confirmed,
                )
                .await?,
            );
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
    Ok(SwapFacts {
        confirmed,
        timestamp: tip.timestamp,
        nonce: observed.nonce(),
        orders,
    })
}

/// Whether the page's logs could hold evidence for an order that its recorded
/// observations and the confirmed nonce `nonce` don't already settle. An executed
/// pre-hook and a post-hook shield count only where the hook's nonce passed, so a
/// hook whose nonce is unused, or used by a recorded winner, has nothing to find. A
/// recorded trade still awaits its reshield, and a newly reached `validTo` is read
/// once in full. Trades of unfilled orders are left to [`any_order_filled`].
fn page_may_hold_hook_evidence(
    record: &ExecutorRecord,
    orders: &[SwapOrderRecord],
    facts: &[OrderFacts],
    nonce: U256,
    timestamp: u64,
) -> bool {
    let explained = |used: U256| {
        record.issued().iter().any(|payload| {
            payload.nonce() == used
                && record.payload_status(payload.hash()) == Some(ExecutorPayloadStatus::Executed)
        }) || orders.iter().zip(facts).any(|(order, facts)| {
            (order.pre_hook().nonce() == used && facts.retained.pre_hook_executed.is_some())
                || (order.post_hook().is_some_and(|hook| hook.nonce() == used)
                    && facts.retained.post_hook_evidence())
        })
    };
    orders.iter().zip(facts).any(|(order, facts)| {
        let retained = &facts.retained;
        let awaiting_delivery = retained.traded.is_some() && retained.delivered.is_none();
        awaiting_delivery
            || (retained.pre_hook_executed.is_none()
                && retained.pre_hook_dead.is_none()
                && nonce > order.pre_hook().nonce())
            || order.post_hook().is_some_and(|hook| {
                !retained.post_hook_evidence() && nonce > hook.nonce() && !explained(hook.nonce())
            })
            || (retained.traded.is_none()
                && retained.pre_hook_dead.is_none()
                && retained.expired.is_none()
                && timestamp > u64::from(order.valid_to()))
    })
}

/// Whether the page may hold a trade of an order without a recorded trade: its
/// `filledAmount` at the confirmed block is nonzero, or the confirmed block is past
/// its `validTo` and the page starts at or before it. A trade sets the amount and
/// needs a block timestamp at or before `validTo`, but the settlement may clear the
/// amount of an order past `validTo`, so a zero amount then no longer shows that
/// earlier blocks hold no trade. Each amount read is kept in `facts` for the expiry
/// check.
async fn any_order_filled(
    provider: &DynProvider,
    settlement: Address,
    orders: &[SwapOrderRecord],
    facts: &mut [OrderFacts],
    block: BlockId,
    headers: &mut Headers<'_>,
    page_start: u64,
) -> Result<bool> {
    let tip = headers.confirmed;
    let confirmed = headers.at(tip).await?.timestamp;
    for (order, facts) in orders.iter().zip(facts) {
        if facts.retained.traded.is_some() {
            continue;
        }
        let filled = filled_amount(provider, settlement, order.uid(), block).await?;
        facts.filled = Some(filled);
        let valid_to = u64::from(order.valid_to());
        if !filled.is_zero()
            || (confirmed > valid_to && headers.at(page_start).await?.timestamp <= valid_to)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn filled_amount(
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

/// Whether the executor's execution nonce moved past `nonce` in `block`, so a payload at
/// `nonce` ran there.
async fn nonce_passed_in(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    executor: Address,
    headers: &mut Headers<'_>,
    block: BlockNumHash,
    nonce: U256,
) -> Result<bool> {
    let parent = headers.at(block.number).await?.parent;
    let before = execution_nonce_at(
        provider,
        chain,
        executor,
        BlockNumHash::new(block.number.saturating_sub(1), parent),
    )
    .await;
    let after = execution_nonce_at(provider, chain, executor, block).await;
    let (Some(before), Some(after)) = (before, after) else {
        return Err(eyre!("executor nonce around a swap hook is unavailable"));
    };
    Ok(before <= nonce && nonce < after)
}

/// Combine still-canonical observations with this read. Observations that rest
/// on state carry the confirmed block, so a reorg of that block reopens them.
fn derive_swap_observations(
    record: &ExecutorRecord,
    railgun: Address,
    facts: &SwapFacts,
) -> Vec<(OrderUid, SwapOrderObservations)> {
    let Some(swap) = record.swap() else {
        return Vec::new();
    };
    let finalized = SwapObservation {
        block: facts.confirmed,
        transaction_hash: None,
    };
    // A later post-hook can deliver an earlier order only for the same token and recipient.
    let shields = facts
        .orders
        .iter()
        .zip(swap.orders())
        .filter_map(|(facts, order)| {
            facts
                .shielded
                .or(facts.retained.shielded)
                .map(|shield| (swap.order_terms(order), shield))
        })
        .collect::<Vec<_>>();
    swap.orders()
        .iter()
        .zip(&facts.orders)
        .map(|(order, observed)| {
            let valid_to = u64::from(order.valid_to());
            let nonce = order.pre_hook().nonce();
            let mut next = observed.retained;
            next.pre_hook_executed = next.pre_hook_executed.or(observed.executed);
            // Retained amounts belong to the retained trade; new ones to the trade just read.
            if next.traded.is_none() {
                next.traded = observed.traded;
                next.trade_amounts = observed.trade_amounts;
            }
            next.shielded = next.shielded.or(observed.shielded);
            if let Some(traded) = next.traded
                && next.delivered.is_none()
            {
                match (order.delivery(), observed.buy_balance) {
                    // The order UID commits to the receiver, and the settlement pays it in the
                    // transaction that emits the matching Trade.
                    (SwapDelivery::External { .. }, _) => next.delivered = Some(traded),
                    // A skipped or early funded post-hook leaves at least buyAmount from
                    // the fill, and so does an Across post-hook that didn't deposit it.
                    (SwapDelivery::Reshield | SwapDelivery::Bridge(_), Some(balance))
                        if balance >= order.bounds().buy_amount =>
                    {
                        next.undelivered = next.undelivered.or(Some(finalized));
                    }
                    // A smaller remainder may be a later gift. Require a matching shield that
                    // actually credited the approved private minimum as well.
                    (SwapDelivery::Reshield, Some(_)) => {
                        next.delivered = shields
                            .iter()
                            .find(|(terms, shield)| {
                                terms.buy_token() == swap.order_terms(order).buy_token()
                                    && terms.recipient() == swap.order_terms(order).recipient()
                                    && shield.observation.block.number >= traded.block.number
                                    && shield.private_amount >= order.bounds().private_minimum
                            })
                            .map(|(_, shield)| SwapObservation {
                                block: facts.confirmed,
                                transaction_hash: shield.observation.transaction_hash,
                            });
                    }
                    // Only the settlement receipt records a Bridge hand-off; explicit
                    // reconciliation doesn't look for one.
                    (SwapDelivery::Reshield | SwapDelivery::Bridge(_), None)
                    | (SwapDelivery::Bridge(_), Some(_)) => {}
                }
            }
            if next.traded.is_none()
                && facts.timestamp > valid_to
                && observed
                    .filled
                    .is_some_and(|filled| filled != order.bounds().sell_amount)
            {
                next.expired = next.expired.or(Some(finalized));
            }
            if next.pre_hook_executed.is_some() {
                next.pre_hook_dead = None;
            } else if let Some(death) = &mut next.pre_hook_dead {
                if death.cause == SwapPreHookDeathCause::Unknown {
                    death.cause = death_cause(record, railgun, nonce, facts);
                }
            } else if facts.nonce > nonce {
                if observed.nullifiers_spent == Some(false) {
                    next.pre_hook_dead = Some(SwapPreHookDeath {
                        cause: death_cause(record, railgun, nonce, facts),
                        observation: finalized,
                    });
                }
            } else if facts.timestamp > valid_to {
                next.pre_hook_dead = Some(SwapPreHookDeath {
                    cause: SwapPreHookDeathCause::Expired,
                    observation: finalized,
                });
            }
            (order.uid(), next)
        })
        .collect()
}

/// What consumed a pre-hook's nonce while its nullifiers stayed unspent.
fn death_cause(
    record: &ExecutorRecord,
    railgun: Address,
    nonce: U256,
    facts: &SwapFacts,
) -> SwapPreHookDeathCause {
    // A payload sent to the executor directly, reconciled as the nonce's winner.
    if let Some(winner) = record.issued().iter().find(|payload| {
        payload.nonce() == nonce
            && record.payload_status(payload.hash()) == Some(ExecutorPayloadStatus::Executed)
    }) {
        return match winner.purpose() {
            ExecutorPayloadPurpose::Recovery if recovers_assets(record, railgun, winner) => {
                SwapPreHookDeathCause::Recovery
            }
            ExecutorPayloadPurpose::Recovery => SwapPreHookDeathCause::Cancellation,
            ExecutorPayloadPurpose::SwapPostHook => SwapPreHookDeathCause::OlderPostHook,
            ExecutorPayloadPurpose::Operation | ExecutorPayloadPurpose::SwapPreHook => {
                SwapPreHookDeathCause::Unknown
            }
        };
    }
    // Inside a settlement, a post-hook is identified by its shield, or its recorded Across
    // deposit, in the block where the nonce passed the post-hook's.
    let older_post_hook = record.swap().is_some_and(|swap| {
        swap.orders()
            .iter()
            .zip(&facts.orders)
            .any(|(order, observed)| {
                order.post_hook().is_some_and(|hook| hook.nonce() == nonce)
                    && (observed.shielded.is_some()
                        || observed.retained.post_hook_deposit.is_some())
            })
    });
    if older_post_hook {
        SwapPreHookDeathCause::OlderPostHook
    } else {
        SwapPreHookDeathCause::Unknown
    }
}

/// Early cancellation is recovery with no assets: it shields nothing.
fn recovers_assets(
    record: &ExecutorRecord,
    railgun: Address,
    payload: &IssuedExecutorPayload,
) -> bool {
    let data = payload.context().calldata();
    let calls = RelayAdapt7702::executeCall::abi_decode(data)
        .map(|call| call._actionData.calls)
        .or_else(|_| RelayAdapt7702::multicallCall::abi_decode(data).map(|call| call._calls));
    match (calls, record.address()) {
        (Ok(calls), Some(executor)) => {
            expected_shields(executor, railgun, &calls).is_ok_and(|shields| !shields.is_empty())
        }
        _ => false,
    }
}

pub(super) fn issued(record: &ExecutorRecord, hash: B256) -> Result<&IssuedExecutorPayload> {
    record
        .issued()
        .iter()
        .find(|payload| payload.hash() == hash)
        .ok_or_else(|| eyre!("swap hook payload is not recorded"))
}

fn spends_all(transactions: &[Transaction]) -> bool {
    !transactions.is_empty()
        && transactions
            .iter()
            .all(|transaction| !transaction.nullifiers.is_empty())
}

fn located(log: &Log, range: &Range<u64>) -> Result<(BlockNumHash, B256)> {
    match (log.block_number, log.block_hash, log.transaction_hash) {
        (Some(number), Some(hash), Some(transaction))
            if range.contains(&number) && !log.removed =>
        {
            Ok((BlockNumHash::new(number, hash), transaction))
        }
        _ => Err(eyre!("swap log is outside the requested canonical page")),
    }
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

/// Stops at the first unspent nullifier: one is enough to show the pre-hook didn't run.
async fn nullifiers_spent(
    provider: &DynProvider,
    railgun: Address,
    transactions: &[Transaction],
    block: BlockId,
) -> Result<bool> {
    for transaction in transactions {
        for nullifier in &transaction.nullifiers {
            let spent = call_at(
                provider,
                railgun,
                SwapRailgun::nullifiersCall {
                    treeNumber: U256::from(transaction.boundParams.treeNumber),
                    nullifier: *nullifier,
                },
                block,
            )
            .await?;
            if !spent {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

async fn call_at<C: SolCall>(
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
