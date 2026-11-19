use super::*;

/// Combine still-canonical observations with this read. Observations that rest
/// on state carry the confirmed block, so a reorg of that block reopens them.
pub(super) fn derive_swap_observations(
    record: &ExecutorRecord,
    railgun: Address,
    sync: Option<&WalletCurrentSnapshot>,
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
            let mut next = observed.retained.clone();
            next.pre_hook_executed = next.pre_hook_executed.or(observed.executed);
            // Retained amounts belong to the retained trade; new ones to the trade just read.
            if next.traded.is_none() {
                next.traded = observed.traded;
                next.trade_amounts = observed.trade_amounts;
            }
            next.shielded = next.shielded.or(observed.shielded);
            // A trade delivered by another attempt of its use keeps that attempt's delivery.
            if let Some(traded) = next.traded
                && next.delivered.is_none()
                && !observed.delivered_by_use
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
                    | (SwapDelivery::Bridge(_) | SwapDelivery::NonEvmBridge(_), _) => {}
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
                    death.cause = death_cause(record, railgun, sync, order, observed, facts);
                }
            } else if observed.funding_after_valid_to {
                // The order had expired when the funding ran, so its own pre-hook never can.
                next.pre_hook_dead = Some(SwapPreHookDeath {
                    cause: SwapPreHookDeathCause::Expired,
                    observation: finalized,
                });
            } else if facts.nonce > nonce {
                // A funding at this nonce ran the shared proof, whichever attempt's hook
                // it was, so the nonce's consumption ends none of them.
                if observed.nullifiers_spent == Some(false)
                    && !shares_funding(&facts.fundings, order, &observed.retained)
                {
                    next.pre_hook_dead = Some(SwapPreHookDeath {
                        cause: death_cause(record, railgun, sync, order, observed, facts),
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

/// What consumed a pre-hook's nonce while its nullifiers stayed unspent. The action
/// attributed at that nonce names it: a recorded spend or a received shield of a payload
/// signed there, then `order` reading as invalidated in the read that showed its nullifiers
/// unspent. Nothing is concluded from another action's missing evidence.
fn death_cause(
    record: &ExecutorRecord,
    railgun: Address,
    sync: Option<&WalletCurrentSnapshot>,
    order: &SwapOrderRecord,
    observed: &OrderFacts,
    facts: &SwapFacts,
) -> SwapPreHookDeathCause {
    let nonce = order.pre_hook().nonce();
    // `invalidateOrder` sets the order's filled amount to the maximum. A death recorded with
    // an unknown cause was itself read from unspent nullifiers after the nonce passed.
    let unspent = observed.nullifiers_spent == Some(false)
        || observed
            .retained
            .pre_hook_dead
            .is_some_and(|death| death.cause == SwapPreHookDeathCause::Unknown);
    let invalidated = (observed.filled == Some(U256::MAX) && unspent).then(|| order.uid());
    let evidence = ExecutorAttributionEvidence {
        railgun,
        sync,
        invalidated_orders: Some(InvalidatedSwapOrders {
            settlement: facts.settlement,
            orders: invalidated.as_slice(),
        }),
    };
    // Only a recovery-kind action is named this way. An older post-hook keeps its own
    // evidence below, which a shield anyone can fund does not replace.
    if let Some(action) = attributed_action(record, nonce, &evidence)
        && action.purpose() == ExecutorPayloadPurpose::Recovery
    {
        // Fee rounds share their calls, so any payload of the action tells.
        let recovers = action
            .payloads()
            .first()
            .and_then(|hash| issued(record, *hash).ok())
            .is_some_and(|payload| recovers_assets(record, railgun, payload));
        return if recovers {
            SwapPreHookDeathCause::Recovery
        } else {
            SwapPreHookDeathCause::Cancellation
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
