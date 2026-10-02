//! Routine settlement confirmation sends only block identifiers to RPC. Private sync's
//! spend location of the pre-hook inputs and the orderbook's trade block are hints; the
//! receipt's trade establishes the outcome, together with its private credit for Reshield
//! delivery, or its Across deposit for Bridge delivery.

use alloy::network::{AnyRpcBlock, ReceiptResponse as _, primitives::HeaderResponse as _};
use alloy::primitives::{Address, U256, keccak256};
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
use crate::ExecutorOwner;
use crate::block_observer::fetch_checked_block_receipts;
use crate::cow::CowOrderbookClient;
use crate::desktop::executor_observation::{expected_shields, trace_step};
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    AcrossOrderTerms, BridgeDelivery, BridgeOrderTerms, BridgeSurplus, ExecutorOperationId,
    ExecutorRecord, SwapBridgeHandoff, SwapDelivery, SwapObservation, SwapOrderRecord,
    SwapShieldObservation, SwapTradeAmounts,
};

// The pinned shared ERC20 bindings do not expose the Transfer event.
alloy::sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
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
    ) -> Result<()> {
        let record = self
            .swap_record(operation)?
            .ok_or_else(|| eyre!("swap is unavailable"))?;
        let order = record
            .swap()
            .and_then(|swap| swap.orders().iter().find(|order| order.uid() == uid))
            .ok_or_else(|| eyre!("swap order is unavailable"))?;
        if order.observations().delivered.is_some() {
            return Ok(());
        }
        for endpoint in self.endpoints.providers().await {
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
                    self.endpoints.succeeded(&endpoint);
                    let _guard = self.lock_activity().await;
                    self.require_record_unchanged(&record)?;
                    self.store.record_swap_settlement(
                        operation,
                        uid,
                        settlement.trade,
                        settlement.amounts,
                        settlement.credit,
                        settlement.handoff,
                    )?;
                    self.notify_change();
                    return Ok(());
                }
                Ok(None) => {
                    self.endpoints.succeeded(&endpoint);
                    return Ok(());
                }
                Err(error) => self.endpoints.failed(&endpoint, &error),
            }
        }
        Err(eyre!(
            "Settlement verification is unavailable. The swap will be checked again."
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
}

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
    .map_err(|_| eyre!("whole-block settlement receipts are incomplete or unavailable"))?;
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
    let shields = match order.post_hook() {
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
            if log.address() != settlement {
                continue;
            }
            let Ok(trade) = log.log_decode::<SwapSettlement::Trade>() else {
                continue;
            };
            let trade = trade.inner.data;
            if trade.owner != executor
                || trade.orderUid != order.uid().0[..]
                || trade.sellToken != terms.sell_token()
                || trade.buyToken != buy_token
            {
                continue;
            }
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
            let mut payouts = logs
                .iter()
                .enumerate()
                .filter(|(_, log)| log.address() == terms.buy_token())
                .filter_map(|(index, log)| Some((index, log.log_decode::<Transfer>().ok()?)))
                .filter(|(_, log)| {
                    log.inner.data.from == settlement
                        && log.inner.data.to == executor
                        && log.inner.data.value == trade.buyAmount
                })
                .map(|(index, _)| index);
            let payout = match (payouts.next(), payouts.next()) {
                (Some(index), None) if index > trade_index && !trade.buyAmount.is_zero() => {
                    Some(index)
                }
                _ => None,
            };
            let credits = |start: usize| {
                shield_credits(logs, start, &shields, terms.buy_token(), executor, railgun).map(
                    move |(private_amount, fee)| SwapShieldObservation {
                        observation,
                        private_amount,
                        fee,
                    },
                )
            };
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
                    let mut deposits = logs
                        .iter()
                        .enumerate()
                        .skip(start)
                        .filter(|(_, log)| spoke_pool == Some(log.address()))
                        .filter_map(|(index, log)| {
                            Some((index, log.log_decode::<SpokePool::FundsDeposited>().ok()?))
                        })
                        .filter(|(index, log)| {
                            signed_deposit(&log.inner.data, across, delivery, executor)
                                && executor_funded(&logs[start..*index], across, executor)
                        });
                    match (deposits.next(), deposits.next()) {
                        (None, _) => (None, None),
                        (Some((index, deposit)), None) => (
                            // Reshielded surplus has no minimum and isn't needed for the
                            // hand-off.
                            if delivery.surplus == BridgeSurplus::Reshield {
                                credits(index + 1).max_by_key(|credit| credit.private_amount)
                            } else {
                                None
                            },
                            Some(SwapBridgeHandoff {
                                observation,
                                deposit_id: Some(deposit.inner.data.depositId),
                            }),
                        ),
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
                amounts: SwapTradeAmounts {
                    sell_amount: trade.sellAmount,
                    buy_amount: trade.buyAmount,
                    fee_amount: trade.feeAmount,
                    // The settlement's cost comes from the receipt already read, the one
                    // that emits this Trade.
                    settlement_gas_used: Some(receipt.gas_used()),
                    settlement_effective_gas_price: Some(receipt.effective_gas_price()),
                    executed_fee: None,
                    executed_fee_token: None,
                },
                credit,
                handoff,
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
