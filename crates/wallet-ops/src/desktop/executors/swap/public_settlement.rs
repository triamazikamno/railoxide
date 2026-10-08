//! What became of the order a Public account placed for a swap, read on the chain it pays on.
//!
//! The record is on the destination chain's owner, which reads the origin chain through that
//! chain's configuration. The orderbook's trade block is a hint. The trade, the settlement's
//! payout to the account's cow-shed proxy and the deposit the order's hook made are read from
//! that block's finalized whole receipts, never from a receipt asked for by hash. A settlement
//! whose hook failed leaves the proceeds with the proxy. The order's hook batch outlives the
//! order, so a deposit can still follow until the batch's deadline: one query for the Public
//! account's deposits, from the settlement to the first final block past that deadline,
//! settles whether it did. The proxy's balance is never read here.
//!
//! A withdrawal whose inclusion was never recorded or left the canonical chain is established
//! without its hash reaching the RPC, from the bought token's transfers from the proxy to the
//! Public account. Every invalidation is established from the settlement's finalized
//! `filledAmount` of the order, even when its sending step recorded an inclusion.

use std::time::{SystemTime, UNIX_EPOCH};

use alloy::eips::{BlockId, BlockNumHash};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{DynProvider, Provider as _};
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::{SolCall as _, SolEvent as _};
use broadcaster_core::contracts::cow::OrderUid;
use broadcaster_core::contracts::cow_shed::{COWShedFactory, decode_withdrawal_calls};
use eyre::{Result, eyre};
use tracing::Instrument as _;

use super::destination::{finalized_receipts, still_canonical};
use super::observation::{MAX_OBSERVATION_BLOCKS, filled_amount};
use super::public_transactions::{DepositorSearch, ExpectedHandoff, find_handoff_by_depositor};
use super::settlement::{ExpectedTrade, Transfer, order_trade, settlement_payout, trade_amounts};
use crate::ExecutorOwner;
use crate::cow::CowOrderbookClient;
use crate::desktop::executor_observation::{ObservationEndpoints, trace_step};
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    ExecutorOperationId, ExecutorRecord, PublicSwapDeposited, PublicSwapInclusion,
    PublicSwapObservations, PublicSwapProxyHolding, PublicSwapRecord, PublicSwapTransactionKind,
    SwapBridgeHandoff, SwapObservation, SwapTradeAmounts, SwapUseId,
};

/// Where a Public-paid order stands on its own chain, from its record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicSwapOrderState {
    /// Submitted, with nothing recorded of its end. Past its `validTo` it awaits the final
    /// block that shows whether it filled.
    Open,
    /// A final block past its validity showed it had not filled.
    Expired,
    /// Invalidated on chain with no trade reported. `retry_at` is when its hook batch can no
    /// longer run.
    Cancelled { retry_at: u64 },
    /// Traded and its proceeds were deposited into Across.
    Bridged,
    /// Traded, not deposited: the proxy holds `amount`. `batch_live` while the batch's deadline
    /// hasn't been ruled out.
    HeldByProxy { amount: U256, batch_live: bool },
    /// Traded, not deposited, and withdrawn to the Public account.
    SwappedNotBridged,
}

/// The state of `swap`'s order, or `None` unless its path is an order.
///
/// A recorded hand-off is `Bridged` whatever else is recorded. Then a confirmed withdrawal is
/// `SwappedNotBridged`, and proceeds the proxy holds are `HeldByProxy`. A trade always wins
/// over a cancellation and an expiry, and a cancellation over an expiry. An order is `Expired`
/// only once a final block past its `validTo` was recorded: a fill in its last moments is
/// read after that time, so until then the order stays `Open`. The store's admission is
/// stricter and stops counting an order as able to fill once its `validTo` passes.
/// `Cancelled::retry_at` is the batch's deadline plus one, the value that admission reports as
/// the time another order may buy the same token.
#[must_use]
pub fn public_swap_order_state(swap: &PublicSwapRecord) -> Option<PublicSwapOrderState> {
    let order = swap.order()?;
    let observed = swap.observations();
    Some(if observed.bridge_handoff.is_some() {
        PublicSwapOrderState::Bridged
    } else if observed.withdrawn.is_some() {
        PublicSwapOrderState::SwappedNotBridged
    } else if let Some(held) = observed.held_by_proxy {
        PublicSwapOrderState::HeldByProxy {
            amount: held.amount,
            batch_live: observed.deposit_ruled_out.is_none(),
        }
    } else if observed.traded.is_some() {
        // A trade is recorded with its hand-off or its payout, so this is a record written
        // by hand. It is neither cancelled nor expired.
        PublicSwapOrderState::Open
    } else if observed.cancelled.is_some() {
        PublicSwapOrderState::Cancelled {
            retry_at: u64::from(order.batch().deadline()) + 1,
        }
    } else if observed.expired.is_some() {
        PublicSwapOrderState::Expired
    } else {
        PublicSwapOrderState::Open
    })
}

/// The current time in Unix seconds.
pub(super) fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| eyre!("the system clock is before the Unix epoch"))?
        .as_secs())
}

impl ExecutorOwner {
    /// Read what became of a Public-paid order on its own chain and record it: its trade, the
    /// payout to the proxy, the deposit its hook made, its expiry, or the deposit a late run of
    /// its batch made. Returns the record afterwards.
    ///
    /// Each call advances as far as finalized evidence allows, from the record alone, and a
    /// call that finds nothing new changes nothing. Withdrawals and invalidations are concluded
    /// only from finalized canonical evidence, including when their inclusion was never
    /// recorded. A swap without an order is returned as it is.
    pub async fn observe_public_swap_order(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        orderbook: &CowOrderbookClient,
    ) -> Result<ExecutorRecord> {
        Box::pin(self.advance_public_swap_order(operation, swap_use, origin, Some(orderbook))).await
    }

    /// [`Self::observe_public_swap_order`], with the orderbook only when the caller has one.
    /// Without it nothing is concluded about an order that hasn't traded: no trade is looked
    /// up, and neither an expiry nor a cancellation is recorded.
    pub(super) async fn advance_public_swap_order(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        orderbook: Option<&CowOrderbookClient>,
    ) -> Result<ExecutorRecord> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        let swap = &claimed.swap;
        let (Some(order), Some(terms)) = (swap.order(), swap.bridge()) else {
            return self.public_swap_record(operation);
        };
        let observed = swap.observations();
        let uid = order.uid();
        // The orderbook is asked only while no trade is recorded. `Some(None)` is its answer
        // that the order has no trade.
        let reported = match orderbook {
            Some(orderbook) if observed.traded.is_none() => {
                Some(self.public_swap_trade_block(orderbook, uid).await?)
            }
            _ => None,
        };
        let settlement = origin
            .public_swap_profile()
            .ok_or_else(|| eyre!("swaps from a Public account are unavailable on this network"))?
            .settlement();
        let withdrawals = included(swap, PublicSwapTransactionKind::Withdrawal);
        let lost_withdrawals = lost_withdrawals(swap, claimed.source, order.buy_token());
        let read = OrderRead {
            finality_depth: origin.finality_depth,
            observed,
            trade_block: reported.flatten(),
            expiry: reported == Some(None),
            trade: ExpectedTrade {
                settlement,
                owner: claimed.source,
                uid,
                sell_token: swap.approval().sell_token,
                buy_token: order.buy_token(),
            },
            sell_amount: swap.approval().bounds.sell_amount,
            valid_to: u64::from(order.valid_to()),
            deadline: u64::from(order.batch().deadline()),
            handoff: ExpectedHandoff {
                spoke_pool: claimed.spoke_pool,
                source: claimed.source,
                destination_chain: self.chain.chain_id,
                terms,
            },
            proxy: order.proxy(),
            withdrawals: &withdrawals,
            lost_withdrawals: &lost_withdrawals,
            invalidation_handed_off: swap
                .transactions()
                .iter()
                .any(|transaction| transaction.kind == PublicSwapTransactionKind::Invalidation),
        };
        let mut next = if read.pending() {
            self.read_public_swap_order(origin, &read).await?
        } else {
            observed
        };
        // The orderbook may learn of a fill after the first answer, so it is asked once more
        // before an expiry is recorded. A trade it reports now is read by the next call.
        if next.expired.is_some()
            && observed.expired.is_none()
            && let Some(orderbook) = orderbook
            && self
                .public_swap_trade_block(orderbook, uid)
                .await?
                .is_some()
        {
            next.expired = None;
        }
        if next == observed {
            return self.public_swap_record(operation);
        }

        let _guard = self.lock_activity().await;
        let record = self.public_swap_record(operation)?;
        let current = record
            .public_swap_use(swap_use)
            .map(|(_, swap)| swap.observations());
        // Another observation of the swap wrote meanwhile. This read is repeated by the next
        // call rather than written over it.
        if current != Some(observed) {
            return Ok(record);
        }
        // A verified withdrawal replaces an absent or provisional inclusion with its final,
        // canonical block, including when it was re-included elsewhere after a reorg.
        if let Some(withdrawn) = next.withdrawn
            && let Some(hash) = withdrawn.transaction_hash
            && lost_withdrawals.iter().any(|lost| lost.hash == hash)
        {
            self.store.record_public_swap_inclusion(
                operation,
                swap_use,
                hash,
                PublicSwapInclusion {
                    observation: withdrawn,
                    succeeded: true,
                    finalized: true,
                },
            )?;
        }
        let record = self
            .store
            .record_public_swap_observations(operation, swap_use, next)?;
        self.notify_change();
        Ok(record)
    }

    /// The destination account's record, which holds the swap.
    fn public_swap_record(&self, operation: ExecutorOperationId) -> Result<ExecutorRecord> {
        self.swap_account_record(operation)?
            .ok_or_else(|| eyre!("the destination stealth account is unavailable"))
    }

    /// The block of the order's trade as the orderbook reports it. The request carries only
    /// the order UID.
    async fn public_swap_trade_block(
        &self,
        orderbook: &CowOrderbookClient,
        uid: OrderUid,
    ) -> Result<Option<u64>> {
        self.while_active(async { Ok(orderbook.order_trade_block(&uid).await?) })
            .await
    }

    /// What `read` finds on the origin chain, from the first of its endpoints that serves it.
    async fn read_public_swap_order(
        &self,
        origin: &EffectiveChainConfig,
        read: &OrderRead<'_>,
    ) -> Result<PublicSwapObservations> {
        let endpoints = ObservationEndpoints::new(origin, &self.http);
        for endpoint in endpoints.providers().await {
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = endpoint.index);
            let result = trace_step(
                "public_swap_order",
                self.while_active(Box::pin(read_order(&endpoint.provider, read))),
            )
            .instrument(span)
            .await;
            match result {
                Ok(next) => {
                    endpoints.succeeded(&endpoint);
                    return Ok(next);
                }
                Err(error) => endpoints.failed(&endpoint, &error),
            }
        }
        Err(eyre!(
            "Order verification on the network the swap pays on is unavailable. The swap will be checked again."
        ))
    }
}

/// Where the swap's transactions of `kind` were included and succeeded, oldest first.
fn included(swap: &PublicSwapRecord, kind: PublicSwapTransactionKind) -> Vec<SwapObservation> {
    swap.transactions()
        .iter()
        .filter(|transaction| transaction.kind == kind)
        .filter_map(|transaction| transaction.inclusion)
        .filter(|inclusion| inclusion.succeeded)
        .map(|inclusion| inclusion.observation)
        .collect()
}

/// A withdrawal whose final, canonical transfer is not recorded yet.
struct LostWithdrawal {
    hash: B256,
    /// The amount its batch transfers to the Public account.
    amount: U256,
    /// The origin chain's head when it was handed off.
    from_block: Option<u64>,
}

/// The swap's withdrawals of `token` to `source`, each with the amount its recorded batch
/// transfers. Provisional inclusions remain candidates because a reorg can move them. One
/// whose recorded request doesn't decode to such a batch is left out.
fn lost_withdrawals(
    swap: &PublicSwapRecord,
    source: Address,
    token: Address,
) -> Vec<LostWithdrawal> {
    swap.transactions()
        .iter()
        .filter(|transaction| transaction.kind == PublicSwapTransactionKind::Withdrawal)
        .filter_map(|transaction| {
            let call = COWShedFactory::executeHooksCall::abi_decode(
                transaction.transaction.input.input()?,
            )
            .ok()?;
            let (withdrawn, amount) = decode_withdrawal_calls(&call.calls, source).ok()?;
            (call.user == source && withdrawn == token).then_some(LostWithdrawal {
                hash: transaction.hash,
                amount,
                from_block: transaction.submitted_from_block,
            })
        })
        .collect()
}

/// What one read of an order on its origin chain starts from.
struct OrderRead<'a> {
    finality_depth: u64,
    /// What the record holds.
    observed: PublicSwapObservations,
    /// The block the orderbook reports the order's trade in.
    trade_block: Option<u64>,
    /// Whether the orderbook was asked and reported no trade, so an expiry may be recorded.
    expiry: bool,
    trade: ExpectedTrade,
    /// The order's sell amount: a fill-or-kill order's `filledAmount` once it traded.
    sell_amount: U256,
    valid_to: u64,
    /// The hook batch's deadline, Unix seconds.
    deadline: u64,
    handoff: ExpectedHandoff<'a>,
    /// The Public account's cow-shed proxy, which the settlement pays.
    proxy: Address,
    /// Where the account's withdrawals were included and succeeded.
    withdrawals: &'a [SwapObservation],
    lost_withdrawals: &'a [LostWithdrawal],
    /// Whether an invalidation was handed off, with or without a recorded inclusion.
    invalidation_handed_off: bool,
}

impl OrderRead<'_> {
    /// Whether the chain can show anything the record doesn't hold yet.
    const fn pending(&self) -> bool {
        let observed = &self.observed;
        let trade = observed.traded.is_none() && self.trade_block.is_some();
        let expiry = self.expiry && observed.expired.is_none();
        let late_deposit =
            observed.bridge_handoff.is_none() && observed.deposit_ruled_out.is_none();
        let withdrawal = observed.withdrawn.is_none()
            && !(self.withdrawals.is_empty() && self.lost_withdrawals.is_empty());
        trade
            || expiry
            || self.invalidation()
            || (observed.held_by_proxy.is_some() && (late_deposit || withdrawal))
    }

    /// Whether `log` is the bought token's transfer of `amount` from the proxy to the Public
    /// account.
    fn withdraws(&self, log: &Log, amount: U256) -> bool {
        log.address() == self.trade.buy_token
            && log.log_decode::<Transfer>().is_ok_and(|transfer| {
                let transfer = transfer.inner.data;
                transfer.from == self.proxy
                    && transfer.to == self.trade.owner
                    && transfer.value == amount
            })
    }

    /// Whether the settlement's state is read for an invalidation: the orderbook reports
    /// no trade, and neither a cancellation nor an expiry is recorded. An expired order gains
    /// nothing from a cancellation.
    const fn invalidation(&self) -> bool {
        self.expiry
            && self.invalidation_handed_off
            && self.observed.traded.is_none()
            && self.observed.cancelled.is_none()
            && self.observed.expired.is_none()
    }
}

/// The record's observations advanced by what `provider` shows in final blocks. Only block
/// identifiers, the order UID's fill and, at the batch's deadline, log queries naming the
/// Public account reach the RPC.
async fn read_order(
    provider: &DynProvider,
    read: &OrderRead<'_>,
) -> Result<PublicSwapObservations> {
    let mut next = read.observed;
    if next.traded.is_none()
        && let Some(number) = read.trade_block
        && let Some(settled) = read_trade(provider, read, number).await?
    {
        next.traded = Some(settled.trade);
        next.trade_amounts = Some(settled.amounts);
        if let Some((deposit_id, deposited)) = settled.handoff {
            next.bridge_handoff = Some(SwapBridgeHandoff {
                observation: settled.trade,
                deposit_id: Some(deposit_id),
            });
            next.deposited = Some(deposited);
        } else {
            next.held_by_proxy = Some(PublicSwapProxyHolding {
                observation: settled.trade,
                amount: settled.payout,
            });
        }
    }
    // Before the expiry, so that an order invalidated in time is recorded as cancelled.
    if read.invalidation() {
        next.cancelled = read_invalidation(provider, read).await?;
    }
    if read.expiry && next.expired.is_none() {
        next.expired = read_expiry(provider, read).await?;
    }
    if let Some(held) = next.held_by_proxy
        && next.bridge_handoff.is_none()
        && next.deposit_ruled_out.is_none()
    {
        match read_late_deposit(provider, read, held.observation.block.number).await? {
            LateDeposit::Found { handoff, deposited } => {
                next.bridge_handoff = Some(handoff);
                next.deposited = Some(deposited);
            }
            LateDeposit::RuledOut(block) => {
                next.deposit_ruled_out = Some(SwapObservation {
                    block,
                    transaction_hash: None,
                });
            }
            LateDeposit::Undecided => {}
        }
    }
    if next.held_by_proxy.is_some() && next.withdrawn.is_none() {
        next.withdrawn = read_withdrawal(provider, read).await?;
    }
    if next.held_by_proxy.is_some() && next.withdrawn.is_none() {
        next.withdrawn = find_lost_withdrawal(provider, read).await?;
    }
    Ok(next)
}

/// An order's trade in a final settlement, with what the same receipt shows of its proceeds.
struct SettledTrade {
    trade: SwapObservation,
    amounts: SwapTradeAmounts,
    /// What the settlement paid the proxy: the trade's `buyAmount`.
    payout: U256,
    /// The deposit id and amounts of the hook's deposit, when the receipt holds it.
    handoff: Option<(U256, PublicSwapDeposited)>,
}

/// The order's trade in block `number`, once that block is final. `None` while it isn't, when
/// it holds no such trade, or when it left the canonical chain during the read.
async fn read_trade(
    provider: &DynProvider,
    read: &OrderRead<'_>,
    number: u64,
) -> Result<Option<SettledTrade>> {
    let Some((block, receipts)) = finalized_receipts(provider, read.finality_depth, number).await?
    else {
        return Ok(None);
    };
    let mut found = None;
    for receipt in &receipts {
        let logs = receipt.inner.logs();
        for (trade_index, log) in logs.iter().enumerate() {
            let Some(trade) = order_trade(log, &read.trade) else {
                continue;
            };
            if found.is_some() {
                return Err(eyre!("settlement contains ambiguous trade evidence"));
            }
            // The settlement pays the order's receiver, the proxy, in the transaction that
            // emits the trade. The pinned settlement's trade is certain without it, so a
            // receipt that doesn't hold exactly that one payout still records the trade, with
            // the event's `buyAmount` as what the proxy holds. Its deposit isn't read from
            // such a receipt: the query at the batch's deadline decides whether one happened.
            let paid = settlement_payout(
                logs,
                trade_index,
                read.trade.settlement,
                read.trade.buy_token,
                read.proxy,
                trade.buyAmount,
            )
            .is_some();
            found = Some(SettledTrade {
                trade: SwapObservation {
                    block,
                    transaction_hash: Some(receipt.transaction_hash()),
                },
                amounts: trade_amounts(&trade, receipt),
                payout: trade.buyAmount,
                // A deposit that differs from the signed terms, or deposits less than the
                // approved minimum, is not this swap's hand-off.
                handoff: paid.then(|| read.handoff.matches(logs)).flatten(),
            });
        }
    }
    if !still_canonical(provider, block).await? {
        return Ok(None);
    }
    Ok(found)
}

/// The identity and timestamp of block `number`.
async fn header_at(provider: &DynProvider, number: u64) -> Result<(BlockNumHash, u64)> {
    let block = trace_step("public_swap_order_header", async {
        provider.get_block_by_number(number.into()).await
    })
    .await?
    .ok_or_else(|| eyre!("a block of the network the swap pays on is unavailable"))?;
    if block.header.number != number {
        return Err(eyre!(
            "a block of the network the swap pays on has the wrong number"
        ));
    }
    Ok((
        BlockNumHash::new(number, block.header.hash),
        block.header.timestamp,
    ))
}

/// The latest final block's number, or `None` while the chain is shorter than its finality
/// depth.
async fn final_number(provider: &DynProvider, finality_depth: u64) -> Result<Option<u64>> {
    let head = trace_step("public_swap_order_head", provider.get_block_number()).await?;
    Ok(head.checked_sub(finality_depth))
}

/// The final block past the order's `validTo` at which it had not filled. As for an order a
/// stealth account places, the settlement's `filledAmount` at that block decides: a
/// fill-or-kill order that traded reads as its sell amount.
async fn read_expiry(
    provider: &DynProvider,
    read: &OrderRead<'_>,
) -> Result<Option<SwapObservation>> {
    let Some(number) = final_number(provider, read.finality_depth).await? else {
        return Ok(None);
    };
    let (block, timestamp) = header_at(provider, number).await?;
    if timestamp <= read.valid_to {
        return Ok(None);
    }
    let filled = filled_amount(
        provider,
        read.trade.settlement,
        read.trade.uid,
        BlockId::hash_canonical(block.hash),
    )
    .await?;
    if filled == read.sell_amount || !still_canonical(provider, block).await? {
        return Ok(None);
    }
    Ok(Some(SwapObservation {
        block,
        transaction_hash: None,
    }))
}

/// The final block at which the settlement holds the order invalidated. A sending step's
/// inclusion can be provisional, so it never decides cancellation. `invalidateOrder` sets the
/// order's `filledAmount` to the maximum, which no fill reaches. The caller reads this only
/// while the orderbook reports no
/// trade: an order invalidated after it filled reads the same.
async fn read_invalidation(
    provider: &DynProvider,
    read: &OrderRead<'_>,
) -> Result<Option<SwapObservation>> {
    let Some(number) = final_number(provider, read.finality_depth).await? else {
        return Ok(None);
    };
    let (block, _) = header_at(provider, number).await?;
    let filled = filled_amount(
        provider,
        read.trade.settlement,
        read.trade.uid,
        BlockId::hash_canonical(block.hash),
    )
    .await?;
    if filled != U256::MAX || !still_canonical(provider, block).await? {
        return Ok(None);
    }
    Ok(Some(SwapObservation {
        block,
        transaction_hash: None,
    }))
}

/// What the one depositor query at the batch's deadline came to.
enum LateDeposit {
    /// The first block past the deadline isn't final yet, or the read met a chain change.
    Undecided,
    Found {
        handoff: SwapBridgeHandoff,
        deposited: PublicSwapDeposited,
    },
    /// No deposit of the swap up to this block, the first one past the deadline.
    RuledOut(BlockNumHash),
}

/// Whether a late run of the hook batch deposited the proceeds. Nothing is asked until the
/// first block whose timestamp is past the batch's deadline is final: until then the batch can
/// still run. That block is found by bisecting block timestamps, which never decrease, between
/// the settlement's block `from` and the latest final block. The Public account's deposits are
/// then queried from `from` to it, and a match is verified in its block's whole receipts
/// against the signed terms.
async fn read_late_deposit(
    provider: &DynProvider,
    read: &OrderRead<'_>,
    from: u64,
) -> Result<LateDeposit> {
    let Some(latest) = final_number(provider, read.finality_depth)
        .await?
        .filter(|latest| *latest >= from)
    else {
        return Ok(LateDeposit::Undecided);
    };
    let (mut block, timestamp) = header_at(provider, latest).await?;
    if timestamp <= read.deadline {
        return Ok(LateDeposit::Undecided);
    }
    // `block` is always the block `high`, which is past the deadline.
    let (mut low, mut high) = (from, latest);
    while low < high {
        let middle = low + (high - low) / 2;
        let (candidate, timestamp) = header_at(provider, middle).await?;
        if timestamp > read.deadline {
            high = middle;
            block = candidate;
        } else {
            low = middle + 1;
        }
    }
    let search = find_handoff_by_depositor(
        provider,
        read.finality_depth,
        from,
        high,
        None,
        &read.handoff,
    )
    .await?;
    Ok(match search {
        DepositorSearch::Found(found) => LateDeposit::Found {
            handoff: SwapBridgeHandoff {
                observation: SwapObservation {
                    block: found.block,
                    transaction_hash: Some(found.transaction_hash),
                },
                deposit_id: Some(found.deposit_id),
            },
            deposited: found.deposited,
        },
        DepositorSearch::Unresolved => LateDeposit::Undecided,
        DepositorSearch::Absent => {
            if still_canonical(provider, block).await? {
                LateDeposit::RuledOut(block)
            } else {
                LateDeposit::Undecided
            }
        }
    })
}

/// The first of the account's withdrawals whose block is final and still holds it succeeding.
/// The block comes from the step that sent the withdrawal, and its whole receipts are read.
async fn read_withdrawal(
    provider: &DynProvider,
    read: &OrderRead<'_>,
) -> Result<Option<SwapObservation>> {
    for withdrawal in read.withdrawals {
        let Some(hash) = withdrawal.transaction_hash else {
            continue;
        };
        let Some(found) = withdrawal_at(provider, read, withdrawal.block.number, hash).await?
        else {
            continue;
        };
        if found.block == withdrawal.block {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// A lost withdrawal, located by the bought token's transfers from the proxy to the Public
/// account in final blocks, from the block the earliest was handed off at. Each log query
/// names those two addresses and spans at most [`MAX_OBSERVATION_BLOCKS`] blocks. A log only
/// locates a block: the withdrawal counts once that block's whole receipts hold the transfer
/// of its amount in the transaction recorded for it.
async fn find_lost_withdrawal(
    provider: &DynProvider,
    read: &OrderRead<'_>,
) -> Result<Option<SwapObservation>> {
    let Some(from) = read
        .lost_withdrawals
        .iter()
        .filter_map(|lost| lost.from_block)
        .min()
    else {
        return Ok(None);
    };
    let Some(to) = final_number(provider, read.finality_depth)
        .await?
        .filter(|to| *to >= from)
    else {
        return Ok(None);
    };
    let (token, proxy, source) = (read.trade.buy_token, read.proxy, read.trade.owner);
    let mut page_from = from;
    loop {
        let page_to = to.min(page_from.saturating_add(MAX_OBSERVATION_BLOCKS - 1));
        let filter = Filter::new()
            .address(token)
            .event_signature(Transfer::SIGNATURE_HASH)
            .topic1(proxy.into_word())
            .topic2(source.into_word())
            .from_block(page_from)
            .to_block(page_to);
        let logs = trace_step("public_swap_withdrawal_logs", provider.get_logs(&filter)).await?;
        for log in &logs {
            let (Some(number), Some(hash)) = (log.block_number, log.transaction_hash) else {
                continue;
            };
            let located = !log.removed
                && (page_from..=page_to).contains(&number)
                && read.lost_withdrawals.iter().any(|lost| {
                    lost.hash == hash
                        && lost.from_block.is_some_and(|from| number >= from)
                        && read.withdraws(log, lost.amount)
                });
            if located && let Some(found) = withdrawal_at(provider, read, number, hash).await? {
                return Ok(Some(found));
            }
        }
        if page_to >= to {
            return Ok(None);
        }
        page_from = page_to + 1;
    }
}

/// The lost withdrawal `hash` in the finalized receipts of block `number`: the transaction
/// succeeded there and its receipt holds the transfer of its amount. `None` while the block
/// isn't final, when it doesn't hold that, or when it left the canonical chain during the read.
async fn withdrawal_at(
    provider: &DynProvider,
    read: &OrderRead<'_>,
    number: u64,
    hash: B256,
) -> Result<Option<SwapObservation>> {
    let Some((block, receipts)) = finalized_receipts(provider, read.finality_depth, number).await?
    else {
        return Ok(None);
    };
    // `finalized_receipts` returns the block's successful receipts only.
    let held = receipts.iter().any(|receipt| {
        receipt.transaction_hash() == hash
            && read.lost_withdrawals.iter().any(|lost| {
                lost.hash == hash
                    && receipt
                        .inner
                        .logs()
                        .iter()
                        .any(|log| read.withdraws(log, lost.amount))
            })
    });
    if !held || !still_canonical(provider, block).await? {
        return Ok(None);
    }
    Ok(Some(SwapObservation {
        block,
        transaction_hash: Some(hash),
    }))
}
