use super::*;

/// The read-only context of one page that [`read_page`] and each order's reads share.
struct PageReads<'a> {
    provider: &'a DynProvider,
    chain: &'a EffectiveChainConfig,
    record: &'a ExecutorRecord,
    executor: Address,
    railgun: Address,
    settlement: Address,
    range: &'a Range<u64>,
    at_confirmed: BlockId,
    /// The confirmed block's timestamp.
    tip_timestamp: u64,
    /// The executor's execution nonce at the confirmed block.
    confirmed_nonce: U256,
}

/// The page's trade logs, and its Railgun logs grouped by block and transaction.
struct Page {
    trades: Vec<Log>,
    railgun_transactions: BTreeMap<(u64, B256), (BlockNumHash, Vec<Log>)>,
}

/// One order and what [`observe_order`] decoded from it for the order's page and state reads.
struct OrderContext<'a> {
    order: &'a SwapOrderRecord,
    /// The order shares one of the retained fundings.
    funded: bool,
    /// The pre-hook's Railgun transactions.
    pre_hook: Vec<Transaction>,
    /// The shields the post-hook requests, empty for an order without one.
    post_hook_shields: Vec<ShieldRequest>,
    /// The pre-hook's nonce.
    nonce: U256,
    /// The order's `validTo`.
    valid_to: u64,
}

pub(super) async fn read_swap_facts(
    provider: &DynProvider,
    chain: &EffectiveChainConfig,
    record: &ExecutorRecord,
    range: Range<u64>,
    sync: Option<&WalletCurrentSnapshot>,
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

    let mut orders = retained_order_facts(&mut headers, swap).await?;
    mark_delivered_by_use(swap, &mut orders);
    let mut fundings = retained_fundings(&mut headers, swap).await?;

    // State at the confirmed block decides first whether the page's logs can hold
    // anything new. When they can't, the page is covered without reading them.
    let page_needed = page_may_hold_hook_evidence(
        record,
        swap.orders(),
        &orders,
        &fundings,
        observed.nonce(),
        tip.timestamp,
        &ExecutorAttributionEvidence {
            railgun,
            sync,
            invalidated_orders: None,
        },
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
    let reads = PageReads {
        provider,
        chain,
        record,
        executor,
        railgun,
        settlement,
        range: &range,
        at_confirmed,
        tip_timestamp: tip.timestamp,
        confirmed_nonce: observed.nonce(),
    };
    let page = read_page(&reads, page_needed).await?;

    // Pre-hook effects found in this read, for orders of a swap use.
    let mut found = Vec::new();
    for (order, facts) in swap.orders().iter().zip(&mut orders) {
        observe_order(
            &reads,
            &mut headers,
            swap,
            &fundings,
            &page,
            order,
            facts,
            &mut found,
        )
        .await?;
    }
    clear_shared_executions(swap, &mut orders);
    merge_found_fundings(&mut fundings, found);
    mark_fundings_after_valid_to(&mut headers, swap, &mut orders, &fundings).await?;

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
        settlement,
        confirmed,
        timestamp: tip.timestamp,
        nonce: observed.nonce(),
        orders,
        fundings,
    })
}

/// Each order's facts, starting from its recorded observations that are still canonical.
async fn retained_order_facts(
    headers: &mut Headers<'_>,
    swap: &SwapOperationRecord,
) -> Result<Vec<OrderFacts>> {
    let mut orders = Vec::with_capacity(swap.orders().len());
    for order in swap.orders() {
        let recorded = order.observations();
        let pre_hook_dead = headers
            .keep_with(recorded.pre_hook_dead, |death| death.observation)
            .await?;
        let traded = headers.keep(recorded.traded).await?;
        orders.push(OrderFacts {
            retained: SwapOrderObservations {
                pre_hook_executed: headers.keep(recorded.pre_hook_executed).await?,
                traded,
                trade_amounts: recorded.trade_amounts.filter(|_| traded.is_some()),
                delivered: headers.keep(recorded.delivered).await?,
                shielded: headers
                    .keep_with(recorded.shielded, |shield| shield.observation)
                    .await?,
                settlement_credit: headers
                    .keep_with(recorded.settlement_credit, |credit| credit.observation)
                    .await?,
                pre_hook_dead,
                undelivered: headers.keep(recorded.undelivered).await?,
                expired: headers.keep(recorded.expired).await?,
                bridge_handoff: headers
                    .keep_with(recorded.bridge_handoff, |handoff| handoff.observation)
                    .await?,
                post_hook_deposit: headers.keep(recorded.post_hook_deposit).await?,
                // The destination chain's outcome isn't evidence on this chain.
                bridge_outcome: recorded.bridge_outcome,
                bridge_refund: headers.keep(recorded.bridge_refund).await?,
            },
            ..OrderFacts::default()
        });
    }
    Ok(orders)
}

/// Mark the orders whose retained trade another attempt of their swap use delivers.
fn mark_delivered_by_use(swap: &SwapOperationRecord, orders: &mut [OrderFacts]) {
    // This mirrors `SwapOperationRecord::delivery_order` over retained observations instead
    // of recorded ones, so a reorged trade or hand-off pairs nothing.
    let delivered_by_use = swap
        .orders()
        .iter()
        .zip(orders.iter())
        .map(|(order, facts)| {
            facts.retained.delivered.is_none()
                && order.use_id().is_some()
                && facts.retained.traded.is_some_and(|trade| {
                    swap.orders()
                        .iter()
                        .zip(orders.iter())
                        .any(|(other, other_facts)| {
                            other.use_id() == order.use_id()
                                && other_facts.retained.traded.is_none()
                                && other_facts.retained.delivered.is_some()
                                && other_facts
                                    .retained
                                    .bridge_handoff
                                    .is_some_and(|handoff| handoff.observation == trade)
                        })
                })
        })
        .collect::<Vec<_>>();
    for (facts, delivered) in orders.iter_mut().zip(delivered_by_use) {
        facts.delivered_by_use = delivered;
    }
}

/// The swap's recorded fundings that are still canonical.
async fn retained_fundings(
    headers: &mut Headers<'_>,
    swap: &SwapOperationRecord,
) -> Result<Vec<SwapFunding>> {
    let mut fundings = Vec::with_capacity(swap.fundings().len());
    for funding in swap.fundings() {
        if headers.keep(Some(funding.observation)).await?.is_some() {
            fundings.push(*funding);
        }
    }
    Ok(fundings)
}

/// The page's logs, or none when `page_needed` is false.
async fn read_page(reads: &PageReads<'_>, page_needed: bool) -> Result<Page> {
    let PageReads {
        provider,
        executor,
        railgun,
        settlement,
        range,
        ..
    } = *reads;
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
        let (block, transaction) = located(&log, range)?;
        railgun_transactions
            .entry((block.number, transaction))
            .or_insert_with(|| (block, Vec::new()))
            .1
            .push(log);
    }
    Ok(Page {
        trades,
        railgun_transactions,
    })
}

/// Establish what the page and the confirmed block show for one order.
async fn observe_order(
    reads: &PageReads<'_>,
    headers: &mut Headers<'_>,
    swap: &SwapOperationRecord,
    fundings: &[SwapFunding],
    page: &Page,
    order: &SwapOrderRecord,
    facts: &mut OrderFacts,
    found: &mut Vec<SwapFunding>,
) -> Result<()> {
    let PageReads {
        record,
        executor,
        railgun,
        ..
    } = *reads;
    let funded = shares_funding(fundings, order, &facts.retained);
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
    let context = OrderContext {
        order,
        funded,
        pre_hook,
        post_hook_shields,
        nonce: order.pre_hook().nonce(),
        valid_to: u64::from(order.valid_to()),
    };
    attribute_receipt_evidence(reads, headers, swap, order, facts).await?;
    scan_page_transactions(
        reads,
        headers,
        &page.railgun_transactions,
        &context,
        facts,
        found,
    )
    .await?;
    match_trade(reads, headers, &page.trades, order, facts).await?;
    read_confirmed_state(reads, swap, &context, facts).await
}

/// Credit the order's retained receipt evidence to its post-hook where that nonce passed.
async fn attribute_receipt_evidence(
    reads: &PageReads<'_>,
    headers: &mut Headers<'_>,
    swap: &SwapOperationRecord,
    order: &SwapOrderRecord,
    facts: &mut OrderFacts,
) -> Result<()> {
    let PageReads {
        provider,
        chain,
        executor,
        ..
    } = *reads;
    // Receipt confirmation established delivery without identifying RPC reads. During
    // explicit account reuse/recovery, establish the post-hook nonce at that known block.
    if facts.retained.shielded.is_none()
        && let Some(credit) = facts.retained.settlement_credit
        && let Some(post_hook) = order.post_hook()
        && nonce_passed_in(
            provider,
            chain,
            executor,
            headers,
            credit.observation.block,
            post_hook.nonce(),
        )
        .await?
    {
        facts.retained.shielded = Some(credit);
    }
    // Likewise, a receipt's Across deposit is the post-hook's once its nonce passed there,
    // unless another post-hook at that nonce signed the same deposit.
    if facts.retained.post_hook_deposit.is_none()
        && let Some(handoff) = facts.retained.bridge_handoff
        && handoff.deposit_id.is_some()
        && let Some(post_hook) = order.post_hook()
        && !deposit_signed_by_another_post_hook(swap.orders(), order)
        && nonce_passed_in(
            provider,
            chain,
            executor,
            headers,
            handoff.observation.block,
            post_hook.nonce(),
        )
        .await?
    {
        facts.retained.post_hook_deposit = Some(handoff.observation);
    }
    Ok(())
}

/// Find the order's pre-hook effects and post-hook shield in the page's Railgun transactions.
async fn scan_page_transactions(
    reads: &PageReads<'_>,
    headers: &mut Headers<'_>,
    railgun_transactions: &BTreeMap<(u64, B256), (BlockNumHash, Vec<Log>)>,
    context: &OrderContext<'_>,
    facts: &mut OrderFacts,
    found: &mut Vec<SwapFunding>,
) -> Result<()> {
    for ((_, transaction), (block, logs)) in railgun_transactions {
        match_pre_hook_execution(
            reads,
            headers,
            context,
            *block,
            *transaction,
            logs,
            facts,
            found,
        )
        .await?;
        match_post_hook_shield(reads, headers, context, *block, *transaction, logs, facts).await?;
    }
    Ok(())
}

/// Record the order's pre-hook execution, and its funding, where this transaction shows it.
async fn match_pre_hook_execution(
    reads: &PageReads<'_>,
    headers: &mut Headers<'_>,
    context: &OrderContext<'_>,
    block: BlockNumHash,
    transaction: B256,
    logs: &[Log],
    facts: &mut OrderFacts,
    found: &mut Vec<SwapFunding>,
) -> Result<()> {
    let PageReads {
        provider,
        chain,
        executor,
        railgun,
        ..
    } = *reads;
    let OrderContext {
        order,
        funded,
        ref pre_hook,
        nonce,
        valid_to,
        ..
    } = *context;
    if facts.retained.pre_hook_executed.is_none()
        && facts.retained.pre_hook_dead.is_none()
        && facts.executed.is_none()
        && !funded
        && spends_all(pre_hook)
        && private_effects_present(railgun, pre_hook, logs)
    {
        let evidence = headers.evidence(block, transaction).await?;
        // The deadline guard reverts after `validTo`, and only a pre-hook signed at
        // nonce `k` consumes it. A reused proof shares nullifiers with an earlier
        // attempt's pre-hook at another nonce, which these checks exclude. A sped-up
        // order's pre-hook shares both, so this is a candidate until the orders are
        // compared below.
        if headers.at(block.number).await?.timestamp <= valid_to
            && nonce_passed_in(provider, chain, executor, headers, block, nonce).await?
        {
            facts.executed = Some(evidence);
            if let Some(use_id) = order.use_id() {
                found.push(SwapFunding {
                    use_id,
                    pre_hook_nonce: nonce,
                    proof: keccak256(pre_hook.abi_encode()),
                    observation: evidence,
                });
            }
        }
    }
    Ok(())
}

/// Record the order's post-hook shield where this transaction holds it.
async fn match_post_hook_shield(
    reads: &PageReads<'_>,
    headers: &mut Headers<'_>,
    context: &OrderContext<'_>,
    block: BlockNumHash,
    transaction: B256,
    logs: &[Log],
    facts: &mut OrderFacts,
) -> Result<()> {
    let PageReads {
        provider,
        chain,
        executor,
        railgun,
        ..
    } = *reads;
    let OrderContext {
        order,
        ref post_hook_shields,
        ..
    } = *context;
    // Anyone can shield with the post-hook's public request, but only the
    // post-hook itself also moves the nonce past its own in that block.
    // The largest credit is chosen by amount alone; its fee only rides along.
    if let Some(post_hook) = order.post_hook()
        && let Some((private_amount, fee)) = logs
            .iter()
            .filter_map(|log| shielded_amount(railgun, log, post_hook_shields))
            .max_by_key(|(private_amount, _)| *private_amount)
    {
        let evidence = headers.evidence(block, transaction).await?;
        if nonce_passed_in(provider, chain, executor, headers, block, post_hook.nonce()).await? {
            facts.shielded = Some(SwapShieldObservation {
                observation: evidence,
                private_amount,
                fee,
            });
        }
    }
    Ok(())
}

/// Record the page's first trade of the order, unless a trade is already retained.
async fn match_trade(
    reads: &PageReads<'_>,
    headers: &mut Headers<'_>,
    trades: &[Log],
    order: &SwapOrderRecord,
    facts: &mut OrderFacts,
) -> Result<()> {
    let PageReads {
        executor,
        settlement,
        range,
        ..
    } = *reads;
    if facts.retained.traded.is_none() {
        for log in trades {
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
            let (block, transaction) = located(log, range)?;
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
    Ok(())
}

/// Read the state at the confirmed block that the order's evidence leaves open.
async fn read_confirmed_state(
    reads: &PageReads<'_>,
    swap: &SwapOperationRecord,
    context: &OrderContext<'_>,
    facts: &mut OrderFacts,
) -> Result<()> {
    let PageReads {
        provider,
        executor,
        railgun,
        settlement,
        at_confirmed,
        tip_timestamp,
        confirmed_nonce,
        ..
    } = *reads;
    let OrderContext {
        order,
        funded,
        ref pre_hook,
        nonce,
        valid_to,
        ..
    } = *context;
    let executed = facts.retained.pre_hook_executed.or(facts.executed);
    let traded = facts.retained.traded.or(facts.traded);
    if executed.is_none()
        && !funded
        && facts.retained.pre_hook_dead.is_none()
        && confirmed_nonce > nonce
    {
        facts.nullifiers_spent =
            Some(nullifiers_spent(provider, railgun, pre_hook, at_confirmed).await?);
    }
    if facts.filled.is_none()
        && ((traded.is_none() && facts.retained.expired.is_none() && tip_timestamp > valid_to)
            || facts.nullifiers_spent == Some(false)
            || facts
                .retained
                .pre_hook_dead
                .is_some_and(|death| death.cause == SwapPreHookDeathCause::Unknown))
    {
        facts.filled = Some(filled_amount(provider, settlement, order.uid(), at_confirmed).await?);
    }
    // External and NEAR Intents orders pay elsewhere, so the executor's buy balance proves
    // nothing for them.
    if facts.retained.traded.or(facts.traded).is_some()
        && facts.retained.delivered.is_none()
        && !facts.delivered_by_use
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
    Ok(())
}

/// Drop the executed pre-hook that attempts of one use sharing a nonce and proof all show.
fn clear_shared_executions(swap: &SwapOperationRecord, orders: &mut [OrderFacts]) {
    // Attempts of one use that share a pre-hook nonce and proof show the same effects, and
    // only one of their pre-hooks ran. The effects then establish the funding and no hook.
    let shared = swap
        .orders()
        .iter()
        .zip(orders.iter())
        .map(|(order, facts)| {
            order.use_id().is_some()
                && facts.executed.is_some()
                && swap
                    .orders()
                    .iter()
                    .zip(orders.iter())
                    .any(|(other, other_facts)| {
                        other.uid() != order.uid()
                            && other.use_id() == order.use_id()
                            && other.pre_hook().nonce() == order.pre_hook().nonce()
                            && other_facts.executed == facts.executed
                    })
        })
        .collect::<Vec<_>>();
    for (facts, shared) in orders.iter_mut().zip(shared) {
        if shared {
            facts.executed = None;
        }
    }
}

/// Add each found funding whose swap use and pre-hook nonce no funding holds yet.
fn merge_found_fundings(fundings: &mut Vec<SwapFunding>, found: Vec<SwapFunding>) {
    for funding in found {
        if !fundings.iter().any(|known| {
            known.use_id == funding.use_id && known.pre_hook_nonce == funding.pre_hook_nonce
        }) {
            fundings.push(funding);
        }
    }
}

/// Mark the orders whose funding at their use and pre-hook nonce sits past their `validTo`.
async fn mark_fundings_after_valid_to(
    headers: &mut Headers<'_>,
    swap: &SwapOperationRecord,
    orders: &mut [OrderFacts],
    fundings: &[SwapFunding],
) -> Result<()> {
    for (order, facts) in swap.orders().iter().zip(orders) {
        let valid_to = u64::from(order.valid_to());
        for funding in fundings {
            if order.use_id() == Some(funding.use_id)
                && order.pre_hook().nonce() == funding.pre_hook_nonce
                && headers
                    .at(funding.observation.block.number)
                    .await?
                    .timestamp
                    > valid_to
            {
                facts.funding_after_valid_to = true;
            }
        }
    }
    Ok(())
}

/// Whether the page's logs could hold evidence for an order that its recorded
/// observations and the confirmed nonce `nonce` don't already settle. An executed
/// pre-hook and a post-hook shield count only where the hook's nonce passed, so a
/// hook whose nonce is unused, or that `evidence` attributes to another action signed at
/// it, has nothing to find, and neither has a pre-hook that shares one of the retained
/// `fundings`. A recorded trade still awaits its reshield, and a newly
/// reached `validTo` is read once in full. Trades of unfilled orders are left to
/// [`any_order_filled`]. In a swap use whose trade and delivery are retained and whose hook
/// nonces are consumed, the page is not read only to learn which hook ran for an order that
/// is itself complete or ended. An order of that use that still awaits its delivery, or
/// whose pre-hook may have run, is still searched.
fn page_may_hold_hook_evidence(
    record: &ExecutorRecord,
    orders: &[SwapOrderRecord],
    facts: &[OrderFacts],
    fundings: &[SwapFunding],
    nonce: U256,
    timestamp: u64,
    evidence: &ExecutorAttributionEvidence<'_>,
) -> bool {
    let explained = |used: U256| {
        attributed_action(record, used, evidence).is_some_and(|action| {
            !matches!(
                action.purpose(),
                ExecutorPayloadPurpose::SwapPreHook | ExecutorPayloadPurpose::SwapPostHook
            )
        }) || orders.iter().zip(facts).any(|(order, facts)| {
            (order.pre_hook().nonce() == used && facts.retained.pre_hook_executed.is_some())
                || (order.post_hook().is_some_and(|hook| hook.nonce() == used)
                    && facts.retained.post_hook_evidence())
        }) || fundings
            .iter()
            .any(|funding| funding.pre_hook_nonce == used)
    };
    orders.iter().zip(facts).any(|(order, order_facts)| {
        if use_source_resolved(orders, facts, order, nonce) && order_quiet(order_facts) {
            return false;
        }
        let retained = &order_facts.retained;
        let awaiting_delivery = retained.traded.is_some()
            && retained.delivered.is_none()
            && !order_facts.delivered_by_use;
        awaiting_delivery
            || (retained.pre_hook_executed.is_none()
                && retained.pre_hook_dead.is_none()
                && !shares_funding(fundings, order, retained)
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

/// Whether the order's own retained observations leave nothing to find: it holds a trade and
/// its delivery, or it holds no trade and its pre-hook is recorded dead and never executed.
/// A traded and delivered order is quiet even when its pre-hook's execution was never
/// observed, since receipts establish its outcome.
const fn order_quiet(facts: &OrderFacts) -> bool {
    let retained = &facts.retained;
    if retained.traded.is_some() {
        retained.delivered.is_some() || facts.delivered_by_use
    } else {
        retained.pre_hook_dead.is_some() && retained.pre_hook_executed.is_none()
    }
}

/// Whether `order`'s swap use has a retained trade with its delivery, on the traded order or
/// on another attempt of the use, and the confirmed nonce `nonce` is past every hook nonce
/// the use's attempts signed. An order without a swap use is never resolved this way. This
/// is the use-level question only: [`page_may_hold_hook_evidence`] skips an order of such a
/// use only when the order is itself quiet, so one that still awaits its delivery or whose
/// pre-hook may have run is still searched.
fn use_source_resolved(
    orders: &[SwapOrderRecord],
    facts: &[OrderFacts],
    order: &SwapOrderRecord,
    nonce: U256,
) -> bool {
    let Some(use_id) = order.use_id() else {
        return false;
    };
    let attempts = || {
        orders
            .iter()
            .zip(facts)
            .filter(move |(attempt, _)| attempt.use_id() == Some(use_id))
    };
    attempts().any(|(_, facts)| {
        facts.retained.traded.is_some()
            && (facts.retained.delivered.is_some() || facts.delivered_by_use)
    }) && attempts().all(|(attempt, _)| {
        nonce > attempt.pre_hook().nonce()
            && attempt.post_hook().is_none_or(|hook| nonce > hook.nonce())
    })
}

/// An order's Across delivery and the terms its post-hook's deposit signed.
fn across_terms(order: &SwapOrderRecord) -> Option<(BridgeDelivery, &AcrossOrderTerms)> {
    match (order.delivery(), order.bridge()) {
        (SwapDelivery::Bridge(delivery), Some(BridgeOrderTerms::Across(terms))) => {
            Some((delivery, terms))
        }
        _ => None,
    }
}

/// Whether another order's post-hook holds `order`'s post-hook nonce and signed a deposit
/// the `FundsDeposited` event can't tell apart from `order`'s. A deposit that several issued
/// payloads signed names approved terms and no hook, so it doesn't show that `order`'s
/// post-hook took the nonce.
fn deposit_signed_by_another_post_hook(
    orders: &[SwapOrderRecord],
    order: &SwapOrderRecord,
) -> bool {
    let (Some(post_hook), Some((delivery, terms))) = (order.post_hook(), across_terms(order))
    else {
        return false;
    };
    orders.iter().any(|other| {
        other.uid() != order.uid()
            && other
                .post_hook()
                .is_some_and(|hook| hook.nonce() == post_hook.nonce())
            && across_terms(other).is_some_and(|(other_delivery, other_terms)| {
                terms.signs_same_deposit(delivery, other_terms, other_delivery)
            })
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
