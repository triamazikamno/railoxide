//! Routine settlement confirmation sends only block identifiers to RPC. Private sync's
//! spend location of the pre-hook inputs and the orderbook's trade block are hints; the
//! receipt's trade establishes the outcome, together with its private credit for Reshield
//! delivery, or its Across deposit for Bridge delivery. A public receiver's Across deposit may
//! match the terms of another attempt of the same swap use, which then holds the hand-off.

use alloy::eips::BlockNumHash;
use alloy::network::{
    AnyRpcBlock, AnyTransactionReceipt, ReceiptResponse as _, primitives::HeaderResponse as _,
};
use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::providers::{DynProvider, EthGetBlock, Provider as _};
use alloy::rpc::types::Log;
use alloy::sol_types::{SolCall as _, SolEvent as _};
use broadcaster_core::contracts::across::{SpokePool, address_to_bytes32};
use broadcaster_core::contracts::cow::OrderUid;
use broadcaster_core::contracts::railgun::{RelayAdapt7702, Shield, ShieldRequest};
use eyre::{Result, eyre};
use tracing::Instrument as _;

use super::observation::{SwapSettlement, issued, shielded_amount};
use super::order::cow_buy_token;
use crate::block_observer::{BlockReceiptsError, fetch_checked_block_receipts};
use crate::cow::CowOrderbookClient;
use crate::desktop::executor_observation::{expected_shields, trace_step};
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    AcrossOrderTerms, BridgeDelivery, BridgeOrderTerms, BridgeProvider, BridgeSurplus,
    ExecutorOperationId, ExecutorRecord, SwapBridgeHandoff, SwapDelivery, SwapObservation,
    SwapOrderRecord, SwapSettlementEvidence, SwapShieldObservation, SwapTradeAmounts,
};
use crate::{ExecutorOwner, OperationHttpClient};

// The pinned shared ERC20 bindings do not expose the Transfer event.
alloy::sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
}

/// A Bridge order's hand-off as its reported settlement block shows it before finality depth.
/// A hint for display: it is never persisted and never establishes an outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapHandoffHint {
    /// The block that was read.
    pub block: BlockNumHash,
    /// The Across deposit's ID. `None` for a NEAR Intents hand-off.
    pub deposit_id: Option<U256>,
    /// What the settlement handed to the bridge, in bought-token base units: the Across
    /// deposit's input amount (the terms of the attempt the deposit matched), or the whole
    /// trade's buy amount for NEAR Intents.
    pub sent: U256,
}

/// What an order's reported settlement block shows before finality depth. A hint for display:
/// it is never persisted and never establishes an outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapSettlementHint {
    /// The block that was read.
    pub block: BlockNumHash,
    /// The trade's buy amount, in bought-token base units.
    pub bought: U256,
    /// A Reshield order's private credit after the shield fee, in bought-token base units, when
    /// the block shows it. Always `None` for other deliveries.
    pub shielded: Option<U256>,
    /// The transaction of that Reshield credit, when the block shows the credit with one.
    /// Always `None` for other deliveries.
    pub shield_transaction: Option<B256>,
    /// A Bridge order's hand-off, when the block shows it.
    pub handoff: Option<SwapHandoffHint>,
}

impl ExecutorOwner {
    /// Verify a CoW-reported settlement block without sending an order, transaction,
    /// account or nullifier identifier to RPC. Unavailable or inconclusive receipts leave
    /// the order unresolved. Execution nonce admission is checked separately on user action.
    pub async fn observe_swap_settlement(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        number: u64,
        client: &OperationHttpClient,
    ) -> Result<()> {
        let record = self
            .swap_record(operation)?
            .ok_or_else(|| eyre!("swap is unavailable"))?;
        let order = record
            .swap()
            .and_then(|swap| swap.orders().iter().find(|order| order.uid() == uid))
            .ok_or_else(|| eyre!("swap order is unavailable"))?;
        // The trade's delivery is already recorded, on this attempt or another of its use.
        if record
            .swap()
            .is_some_and(|swap| swap.delivery_order(order).is_some())
        {
            return Ok(());
        }
        let endpoints = self.observation_endpoints(client)?;
        for endpoint in endpoints.providers().await {
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = endpoint.index);
            let result = trace_step(
                "swap_settlement_receipts",
                self.while_active(Box::pin(read_settlement(
                    &endpoint.provider,
                    &self.chain,
                    &record,
                    order,
                    number,
                ))),
            )
            .instrument(span)
            .await;
            match result {
                Ok(Some(settlement)) => {
                    endpoints.succeeded(&endpoint);
                    let _guard = self.lock_activity().await;
                    self.require_record_unchanged(&record)?;
                    self.store.record_swap_settlement(
                        operation,
                        uid,
                        &SwapSettlementEvidence {
                            traded: settlement.trade,
                            amounts: settlement.amounts,
                            credit: settlement.credit,
                            handoff: settlement.handoff,
                            delivery: settlement.delivery,
                        },
                    )?;
                    self.notify_change();
                    return Ok(());
                }
                Ok(None) => {
                    endpoints.succeeded(&endpoint);
                    return Ok(());
                }
                Err(error) => endpoints.failed(&endpoint, &error),
            }
        }
        Err(eyre!(
            "Settlement verification is unavailable. The swap will be checked again."
        ))
    }

    /// Read what a CoW-reported settlement block shows for the order without waiting for
    /// finality depth, sending only block identifiers to RPC as the verification does. Nothing
    /// is recorded. `None` when the block was read whole and holds no trade of the order, when
    /// the order's delivery is already recorded, or when every endpoint tried answered that
    /// whole-block receipts are unsupported. Any other failed read is an error, so the caller
    /// asks again.
    pub async fn read_swap_settlement_hint(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        number: u64,
        client: &OperationHttpClient,
    ) -> Result<Option<SwapSettlementHint>> {
        let record = self
            .swap_record(operation)?
            .ok_or_else(|| eyre!("swap is unavailable"))?;
        let swap = record.swap().ok_or_else(|| eyre!("swap is unavailable"))?;
        let order = swap
            .orders()
            .iter()
            .find(|order| order.uid() == uid)
            .ok_or_else(|| eyre!("swap order is unavailable"))?;
        if swap.delivery_order(order).is_some() {
            return Ok(None);
        }
        let mut tried = 0_usize;
        let mut unsupported = 0_usize;
        let endpoints = self.observation_endpoints(client)?;
        for endpoint in endpoints.providers().await {
            tried += 1;
            let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = endpoint.index);
            let result = trace_step(
                "swap_settlement_hint_receipts",
                self.while_active(Box::pin(read_block_settlement(
                    &endpoint.provider,
                    &self.chain,
                    &record,
                    order,
                    number,
                ))),
            )
            .instrument(span)
            .await;
            match result {
                Ok(settlement) => {
                    endpoints.succeeded(&endpoint);
                    return Ok(settlement.map(|settlement| {
                        let handoff = settlement.handoff.map(|handoff| {
                            // The deposit matched the signed input amount of the attempt it is
                            // attributed to.
                            let sent = match swap
                                .orders()
                                .iter()
                                .find(|order| order.uid() == settlement.delivery)
                                .and_then(SwapOrderRecord::bridge)
                            {
                                Some(BridgeOrderTerms::Across(across)) => across.input_amount,
                                _ => settlement.amounts.buy_amount,
                            };
                            SwapHandoffHint {
                                block: handoff.observation.block,
                                deposit_id: handoff.deposit_id,
                                sent,
                            }
                        });
                        // An Across order's reshielded surplus is not the order's delivery.
                        let credit = settlement
                            .credit
                            .filter(|_| matches!(order.delivery(), SwapDelivery::Reshield));
                        SwapSettlementHint {
                            block: settlement.trade.block,
                            bought: settlement.amounts.buy_amount,
                            shielded: credit.map(|credit| credit.private_amount),
                            shield_transaction: credit
                                .and_then(|credit| credit.observation.transaction_hash),
                            handoff,
                        }
                    }));
                }
                Err(error) => {
                    if matches!(
                        error.downcast_ref::<BlockReceiptsError>(),
                        Some(BlockReceiptsError::Unsupported)
                    ) {
                        unsupported += 1;
                    }
                    endpoints.failed(&endpoint, &error);
                }
            }
        }
        if tried > 0 && unsupported == tried {
            return Ok(None);
        }
        Err(eyre!(
            "The settlement block is unavailable. The swap will be checked again."
        ))
    }

    /// Read the orderbook's executed fee of an order whose trade is verified, through the
    /// swap's own orderbook route, and persist it with the trade's amounts. The request carries
    /// only the order UID, and the fee is a hint for display, never an outcome. Nothing is
    /// requested before the trade is recorded or once its fee is, including after a restart.
    /// A failed read records nothing and isn't retried here.
    pub async fn observe_swap_executed_fee(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        orderbook: &CowOrderbookClient,
    ) -> Result<()> {
        let record = self
            .swap_record(operation)?
            .ok_or_else(|| eyre!("swap is unavailable"))?;
        let observations = record
            .swap()
            .and_then(|swap| swap.orders().iter().find(|order| order.uid() == uid))
            .ok_or_else(|| eyre!("swap order is unavailable"))?
            .observations();
        // A trade recorded before amounts were kept has nowhere to keep the fee.
        if observations.traded.is_none()
            || observations
                .trade_amounts
                .is_none_or(|amounts| amounts.executed_fee.is_some())
        {
            return Ok(());
        }
        let fee = self
            .while_active(async { Ok(orderbook.order_executed_fee(&uid).await?) })
            .await?;
        let Some(fee) = fee else {
            return Ok(());
        };
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        self.store
            .record_swap_executed_fee(operation, uid, fee.amount, fee.token)?;
        self.notify_change();
        Ok(())
    }
}

struct Settlement {
    trade: SwapObservation,
    amounts: SwapTradeAmounts,
    /// Reshield's private credit, or the surplus an Across post-hook reshields after its
    /// deposit. Always `None` for External and NEAR Intents delivery.
    credit: Option<SwapShieldObservation>,
    /// A Bridge order's hand-off: the NEAR Intents trade, or the Across deposit that follows
    /// its payout. `None` while an Across post-hook's deposit is missing.
    handoff: Option<SwapBridgeHandoff>,
    /// The attempt whose approved terms the hand-off and credit match. It is the traded order,
    /// except when an Across deposit matches another attempt of the same use.
    delivery: OrderUid,
}

/// [`read_block_settlement`] once block `number` is at finality depth. `None` before that.
async fn read_settlement(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    record: &ExecutorRecord,
    order: &SwapOrderRecord,
    number: u64,
) -> Result<Option<Settlement>> {
    let head = trace_step("settlement_head", provider.get_block_number()).await?;
    if head
        .checked_sub(chain.finality_depth)
        .is_none_or(|safe| number > safe)
    {
        return Ok(None);
    }
    Box::pin(read_block_settlement(
        provider, chain, record, order, number,
    ))
    .await
}

/// What block `number` establishes for `order`, from its whole-block receipts and whatever its
/// depth: `None` when it holds no trade of the order. Only [`read_settlement`]'s result is
/// evidence.
async fn read_block_settlement(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    record: &ExecutorRecord,
    order: &SwapOrderRecord,
    number: u64,
) -> Result<Option<Settlement>> {
    let block = trace_step("settlement_block", async {
        EthGetBlock::<AnyRpcBlock>::by_number(number.into(), provider.client()).await
    })
    .await?
    .ok_or_else(|| eyre!("settlement block is unavailable"))?;
    let identity = block.header.num_hash();
    if identity.number != number {
        return Err(eyre!("settlement block has the wrong number"));
    }
    let hashes = block.transactions.hashes().collect::<Vec<_>>();
    let receipts = trace_step(
        "settlement_receipts",
        fetch_checked_block_receipts(provider, identity, &hashes),
    )
    .await
    .map_err(|error| {
        // Kept apart from a failed read: an early read of the block gives up on an endpoint
        // that doesn't serve whole-block receipts.
        if matches!(error, BlockReceiptsError::Unsupported) {
            eyre::Report::new(error)
        } else {
            eyre!("whole-block settlement receipts are incomplete or unavailable")
        }
    })?;
    let settlement = chain
        .swap_profile()
        .ok_or_else(|| eyre!("swaps are unavailable"))?
        .settlement();
    let railgun = chain.require_railgun()?.deployment.contract;
    // Only this chain's own SpokePool emits deposits that hand an order to Across.
    let spoke_pool = chain.bridge_profile().map(|profile| profile.spoke_pool());
    let executor = record
        .address()
        .ok_or_else(|| eyre!("swap account is unavailable"))?;
    let swap = record.swap().ok_or_else(|| eyre!("swap is unavailable"))?;
    let terms = swap.order_terms(order);
    let buy_token = cow_buy_token(terms.buy_token());
    // An order without a post-hook has no post-hook shield to match.
    let hook_shields = |order: &SwapOrderRecord| -> Result<Vec<ShieldRequest>> {
        match order.post_hook() {
            Some(post_hook) => expected_shields(
                executor,
                railgun,
                &RelayAdapt7702::multicallCall::abi_decode(
                    issued(record, post_hook.payload())?.context().calldata(),
                )?
                ._calls,
            ),
            None => Ok(Vec::new()),
        }
    };
    let shields = hook_shields(order)?;
    let expected = ExpectedTrade {
        settlement,
        owner: executor,
        uid: order.uid(),
        sell_token: terms.sell_token(),
        buy_token,
    };
    let mut found = None;
    for receipt in receipts {
        if !receipt.status() {
            continue;
        }
        let logs = receipt.inner.logs();
        // The enclosing receipt supplies inclusion; inconsistent log metadata is not evidence.
        if logs.iter().any(|log| {
            log.removed
                || log.block_hash != Some(identity.hash)
                || log.block_number != Some(number)
                || log.transaction_hash != Some(receipt.transaction_hash())
        }) {
            return Err(eyre!("settlement receipt logs have inconsistent inclusion"));
        }
        for (trade_index, log) in logs.iter().enumerate() {
            let Some(trade) = order_trade(log, &expected) else {
                continue;
            };
            if found.is_some() {
                return Err(eyre!("settlement contains ambiguous trade evidence"));
            }
            let observation = SwapObservation {
                block: identity,
                transaction_hash: Some(receipt.transaction_hash()),
            };
            // A second matching payout could mean a funded post-hook ran before the
            // actual fill's payout. Do not attribute that earlier credit or deposit to this
            // trade. The single payout must follow the trade.
            let payout = settlement_payout(
                logs,
                trade_index,
                settlement,
                terms.buy_token(),
                executor,
                trade.buyAmount,
            );
            let credit = |(private_amount, fee)| SwapShieldObservation {
                observation,
                private_amount,
                fee,
            };
            let credits = |start: usize| {
                shield_credits(logs, start, &shields, terms.buy_token(), executor, railgun)
                    .map(credit)
            };
            let mut delivery_uid = order.uid();
            // The order UID commits to an External receiver or a NEAR Intents deposit address,
            // and the settlement pays it in the call that emits this Trade, so the trade alone
            // establishes External delivery and the NEAR Intents hand-off. Native buys emit no
            // Transfer.
            let (credit, handoff) = match (order.delivery(), order.bridge()) {
                (SwapDelivery::Reshield, _) => (
                    payout.and_then(|payout| {
                        credits(payout + 1)
                            .filter(|credit| {
                                credit.private_amount >= order.bounds().private_minimum
                                    && credit
                                        .fee
                                        .and_then(|fee| credit.private_amount.checked_add(fee))
                                        .is_some_and(|amount| amount >= trade.buyAmount)
                            })
                            .max_by_key(|credit| credit.private_amount)
                    }),
                    None,
                ),
                (SwapDelivery::Bridge(delivery), Some(BridgeOrderTerms::Across(across))) => {
                    let start = payout.map_or(logs.len(), |payout| payout + 1);
                    // An earlier attempt's post-hook holds the same nonce as this order's, so a
                    // public receiver's deposit may carry the terms of any attempt of the use.
                    let candidates = if delivery.is_private() || order.use_id().is_none() {
                        vec![(order, delivery, across)]
                    } else {
                        swap.orders()
                            .iter()
                            .filter(|candidate| candidate.use_id() == order.use_id())
                            .filter_map(|candidate| {
                                match (candidate.delivery(), candidate.bridge()) {
                                    (
                                        SwapDelivery::Bridge(delivery),
                                        Some(BridgeOrderTerms::Across(across)),
                                    ) if delivery.provider == BridgeProvider::Across
                                        && !delivery.is_private() =>
                                    {
                                        Some((candidate, delivery, across))
                                    }
                                    _ => None,
                                }
                            })
                            .collect()
                    };
                    // Each deposit after the payout that some attempt signed, with the attempt
                    // whose terms it is recorded under: the traded order when it matches, and
                    // otherwise the earliest matching attempt. This tie-break names approved
                    // terms and does not show which hook ran.
                    let mut deposits = logs
                        .iter()
                        .enumerate()
                        .skip(start)
                        .filter(|(_, log)| spoke_pool == Some(log.address()))
                        .filter_map(|(index, log)| {
                            let deposit = log.log_decode::<SpokePool::FundsDeposited>().ok()?;
                            let (owner, delivery, _) = candidates
                                .iter()
                                .filter(|(_, delivery, across)| {
                                    signed_deposit(&deposit.inner.data, across, *delivery, executor)
                                        && executor_funded(&logs[start..index], across, executor)
                                })
                                .min_by_key(|(candidate, ..)| {
                                    (candidate.uid() != order.uid(), candidate.attempt())
                                })?;
                            Some((index, deposit.inner.data.depositId, *owner, *delivery))
                        });
                    match (deposits.next(), deposits.next()) {
                        (None, _) => (None, None),
                        (Some((index, deposit_id, owner, delivery)), None) => {
                            delivery_uid = owner.uid();
                            (
                                // Reshielded surplus has no minimum and isn't needed for the
                                // hand-off. It follows the shield requests the owner signed.
                                if delivery.surplus == BridgeSurplus::Reshield {
                                    shield_credits(
                                        logs,
                                        index + 1,
                                        &hook_shields(owner)?,
                                        terms.buy_token(),
                                        executor,
                                        railgun,
                                    )
                                    .map(credit)
                                    .max_by_key(|credit| credit.private_amount)
                                } else {
                                    None
                                },
                                Some(SwapBridgeHandoff {
                                    observation,
                                    deposit_id: Some(deposit_id),
                                }),
                            )
                        }
                        (Some(_), Some(_)) => {
                            return Err(eyre!("settlement contains ambiguous deposit evidence"));
                        }
                    }
                }
                (SwapDelivery::Bridge(_), Some(BridgeOrderTerms::NearIntents(_))) => (
                    None,
                    Some(SwapBridgeHandoff {
                        observation,
                        deposit_id: None,
                    }),
                ),
                // A Bridge order always carries its provider's terms.
                (SwapDelivery::External { .. } | SwapDelivery::Bridge(_), _) => (None, None),
            };
            found = Some(Settlement {
                trade: observation,
                amounts: trade_amounts(&trade, &receipt),
                credit,
                handoff,
                delivery: delivery_uid,
            });
        }
    }
    if trace_step("settlement_canonical_recheck", async {
        provider.get_block_by_number(number.into()).await
    })
    .await?
    .is_none_or(|block| block.header.num_hash() != identity)
    {
        return Err(eyre!("settlement block changed during verification"));
    }
    Ok(found)
}

/// What identifies an order's trade among a settlement transaction's logs.
pub(super) struct ExpectedTrade {
    /// The chain's pinned settlement contract.
    pub(super) settlement: Address,
    pub(super) owner: Address,
    pub(super) uid: OrderUid,
    pub(super) sell_token: Address,
    /// The buy token as the `Trade` event reports it.
    pub(super) buy_token: Address,
}

/// The `Trade` event of the `expected` order in `log`, which only the pinned settlement emits.
pub(super) fn order_trade(log: &Log, expected: &ExpectedTrade) -> Option<SwapSettlement::Trade> {
    if log.address() != expected.settlement {
        return None;
    }
    let trade = log.log_decode::<SwapSettlement::Trade>().ok()?.inner.data;
    (trade.owner == expected.owner
        && trade.orderUid == expected.uid.0[..]
        && trade.sellToken == expected.sell_token
        && trade.buyToken == expected.buy_token)
        .then_some(trade)
}

/// The index among `logs` of the settlement's payout of the trade at `trade_index`: the one
/// transfer of `amount` of `token` from `settlement` to `receiver`, which must follow the
/// trade. `None` without exactly one, or for a zero amount.
pub(super) fn settlement_payout(
    logs: &[Log],
    trade_index: usize,
    settlement: Address,
    token: Address,
    receiver: Address,
    amount: U256,
) -> Option<usize> {
    let mut payouts = logs
        .iter()
        .enumerate()
        .filter(|(_, log)| log.address() == token)
        .filter_map(|(index, log)| Some((index, log.log_decode::<Transfer>().ok()?)))
        .filter(|(_, log)| {
            log.inner.data.from == settlement
                && log.inner.data.to == receiver
                && log.inner.data.value == amount
        })
        .map(|(index, _)| index);
    match (payouts.next(), payouts.next()) {
        (Some(index), None) if index > trade_index && !amount.is_zero() => Some(index),
        _ => None,
    }
}

/// The executed amounts of `trade`, with the cost of the settlement from `receipt`, the receipt
/// already read that emits it.
pub(super) const fn trade_amounts(
    trade: &SwapSettlement::Trade,
    receipt: &AnyTransactionReceipt,
) -> SwapTradeAmounts {
    SwapTradeAmounts {
        sell_amount: trade.sellAmount,
        buy_amount: trade.buyAmount,
        fee_amount: trade.feeAmount,
        settlement_gas_used: Some(receipt.gas_used()),
        settlement_effective_gas_price: Some(receipt.effective_gas_price()),
        executed_fee: None,
        executed_fee_token: None,
    }
}

/// Credits from the post-hook's `shields` at or after `logs[start]`, which callers place
/// after the payout, or after the Across deposit. An early funded post-hook or a copied
/// public Shield request must not complete the order, so each credit needs a matching debit
/// from this executor to Railgun since `start` and after any earlier shield. Railgun
/// receives the net private amount; the shield fee is transferred separately to its treasury.
fn shield_credits(
    logs: &[Log],
    start: usize,
    shields: &[ShieldRequest],
    token: Address,
    executor: Address,
    railgun: Address,
) -> impl Iterator<Item = (U256, Option<U256>)> {
    logs.iter()
        .enumerate()
        .skip(start)
        .filter_map(move |(index, log)| {
            let (private_amount, fee) = shielded_amount(railgun, log, shields)?;
            debited_then_shielded(
                &logs[start..index],
                token,
                executor,
                railgun,
                private_amount,
            )
            .then_some((private_amount, fee))
        })
}

fn debited_then_shielded(
    logs: &[Log],
    token: Address,
    executor: Address,
    railgun: Address,
    private_amount: U256,
) -> bool {
    let mut shield_debit = false;
    for log in logs {
        if log.address() == railgun && log.topic0() == Some(&Shield::SIGNATURE_HASH) {
            // A debit belonging to an earlier shield cannot establish this credit.
            shield_debit = false;
        }
        if log.address() != token {
            continue;
        }
        let Ok(transfer) = log.log_decode::<Transfer>() else {
            continue;
        };
        let transfer = transfer.inner.data;
        if transfer.to == railgun {
            shield_debit = transfer.from == executor && transfer.value == private_amount;
        }
    }
    shield_debit
}

/// `depositV3` pulls the input from its caller, not from the `depositor` it names, so a
/// deposit only hands off the executor's funds after the executor paid the `SpokePool`.
fn executor_funded(logs: &[Log], terms: &AcrossOrderTerms, executor: Address) -> bool {
    logs.iter()
        .filter(|log| log.address() == terms.input_token)
        .filter_map(|log| log.log_decode::<Transfer>().ok())
        .any(|transfer| {
            let transfer = transfer.inner.data;
            transfer.from == executor
                && transfer.to == terms.spoke_pool
                && transfer.value == terms.input_amount
        })
}

/// Whether `deposit` is the one the Across post-hook signed for this order. The event
/// carries the resolved exclusivity deadline rather than the signed parameter, so that
/// isn't compared. A private delivery's deposit pays Across's handler with the signed message;
/// any other deposit carries no message.
fn signed_deposit(
    deposit: &SpokePool::FundsDeposited,
    terms: &AcrossOrderTerms,
    delivery: BridgeDelivery,
    executor: Address,
) -> bool {
    deposit.depositor == address_to_bytes32(executor)
        && deposit.recipient == address_to_bytes32(terms.deposit_recipient(delivery))
        && match terms.message_hash {
            Some(hash) => keccak256(&deposit.message) == hash,
            None => deposit.message.is_empty(),
        }
        && deposit.destinationChainId == U256::from(delivery.destination_chain)
        && deposit.inputToken == address_to_bytes32(terms.input_token)
        && deposit.outputToken == address_to_bytes32(terms.output_token)
        && deposit.inputAmount == terms.input_amount
        && deposit.outputAmount == terms.output_amount
        && deposit.quoteTimestamp == terms.quote_timestamp
        && deposit.fillDeadline == terms.fill_deadline
        && deposit.exclusiveRelayer == address_to_bytes32(terms.exclusive_relayer)
}
