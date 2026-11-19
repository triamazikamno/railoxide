#[allow(clippy::wildcard_imports)]
use super::*;

impl ExecutorStore {
    /// Save acceptance independently of execution. A late failed request cannot undo
    /// a successful submission of the same immutable order. Sending an order reserves
    /// its hooks' inputs again, even after the user released them.
    pub fn record_swap_submission(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        status: SwapSubmissionStatus,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let order = record
                .swap
                .as_mut()
                .and_then(|swap| swap.orders.iter_mut().find(|order| order.uid == uid.0))
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            if order.submission_status != SwapSubmissionStatus::Accepted {
                order.submission_status = status;
            }
            let hooks = [
                Some(order.pre_hook.payload),
                order.post_hook.map(|hook| hook.payload),
            ];
            if matches!(
                status,
                SwapSubmissionStatus::Pending | SwapSubmissionStatus::Accepted
            ) {
                record
                    .released_payloads
                    .retain(|hash| !hooks.contains(&Some(*hash)));
            }
            Ok(())
        })
    }

    /// Persist the terms approved with a swap's setup, replacing an earlier approval. Only the
    /// expected latest use takes one, as an active source use without orders that was not
    /// stopped; an order records its own bounds.
    pub fn record_swap_approval(
        &self,
        operation: ExecutorOperationId,
        expected_use: SwapUseId,
        approval: SwapApproval,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            if record.address.is_none() || record.swap_setup_stopped {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            if record.active_swap_use != Some(expected_use) {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            let has_orders = record
                .swap_uses
                .last()
                .is_some_and(|swap_use| record.has_swap_use_order(swap_use.id));
            // A setup sent again beside an order placed on it while it was included may raise
            // its own fee limit. Nothing else the order was approved with changes.
            let setup_fee_only = has_orders
                && record.unresolved_swap_setup_nonce(expected_use).is_some()
                && record
                    .swap_uses
                    .last()
                    .and_then(SwapUseRecord::approval)
                    .is_some_and(|known| {
                        let mut unchanged = approval.clone();
                        unchanged.bounds.source_setup_fee = known.bounds.source_setup_fee;
                        unchanged == *known
                    });
            match record.swap_uses.last_mut() {
                Some(SwapUseRecord {
                    id,
                    stopped: false,
                    role:
                        SwapUseRole::Source {
                            approval: known,
                            destination_operation,
                        },
                    ..
                }) if *id == expected_use && (!has_orders || setup_fee_only) => {
                    // Cancellation resolves the linked destination through this delivery's chain.
                    if destination_operation.is_some()
                        && !matches!(
                            (known.as_deref().map(|known| &known.delivery), &approval.delivery),
                            (Some(SwapDelivery::Bridge(previous)), SwapDelivery::Bridge(replacement))
                                if previous.is_private()
                                    && replacement.is_private()
                                    && previous.destination_chain == replacement.destination_chain
                        )
                    {
                        return Err(ExecutorStoreError::OperationMismatch);
                    }
                    *known = Some(Box::new(approval));
                }
                _ => return Err(ExecutorStoreError::OperationMismatch),
            }
            Ok(())
        })
    }

    /// Replace an order's canonical observations, including when a reorg removes
    /// one. Record pre-hook death only at finalized depth, since it admits a retry.
    /// Every observation is at or below the record's reconciled confirmed block, and trade
    /// amounts come only with their trade.
    pub fn record_swap_observations(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        observations: SwapOrderObservations,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let confirmed = record
                .nonce_observation
                .ok_or(ExecutorStoreError::OutstandingNonce)?
                .block;
            if observations
                .blocks()
                .any(|block| block.number > confirmed.number)
                || observations.trade_amounts.is_some() && observations.traded.is_none()
            {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            let order = record
                .swap
                .as_mut()
                .and_then(|swap| swap.orders.iter_mut().find(|order| order.uid == uid.0))
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            order.observations = observations;
            Ok(())
        })
    }

    /// Replace the swap's verified fundings, including when a reorg removes one. Each is at or
    /// below the record's reconciled confirmed block, names the transaction that ran the
    /// proof, and belongs to a recorded order's swap use and pre-hook nonce, one per pair.
    pub fn record_swap_fundings(
        &self,
        operation: ExecutorOperationId,
        fundings: Vec<SwapFunding>,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let confirmed = record
                .nonce_observation
                .ok_or(ExecutorStoreError::OutstandingNonce)?
                .block;
            let same = |one: &SwapFunding, other: &SwapFunding| {
                one.use_id == other.use_id && one.pre_hook_nonce == other.pre_hook_nonce
            };
            if fundings.iter().enumerate().any(|(index, funding)| {
                funding.observation.block.number > confirmed.number
                    || funding.observation.transaction_hash.is_none()
                    || fundings[..index]
                        .iter()
                        .any(|earlier| same(earlier, funding))
            }) {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            let swap = record
                .swap
                .as_mut()
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            if !fundings.iter().all(|funding| {
                swap.orders.iter().any(|order| {
                    order.use_id == Some(funding.use_id)
                        && order.pre_hook.nonce == funding.pre_hook_nonce
                })
            }) {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            swap.fundings = fundings;
            Ok(())
        })
    }

    /// Persist receipt evidence checked at head minus finality depth by the owner.
    /// This deliberately leaves account nonce reconciliation and input reservations alone.
    /// A Reshield order is delivered only with a private credit. An External order is
    /// delivered by its trade and never carries a credit. A Bridge order is delivered on this
    /// chain by its hand-off, in the trade's own transaction: for NEAR Intents the trade, for
    /// Across the deposit that names its id, optionally with the surplus credit of a post-hook
    /// that reshields it. Same-chain orders take no hand-off.
    ///
    /// `uid` is the traded order. Usually it is also the evidence's delivery order and takes
    /// all of it. A public-receiver Across deposit may instead match another attempt of the
    /// same swap use. The traded order then keeps the trade and its amounts, and that attempt
    /// takes the hand-off, `delivered` and any surplus credit, in the same write.
    pub(crate) fn record_swap_settlement(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        evidence: &SwapSettlementEvidence,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let SwapSettlementEvidence {
            traded,
            amounts,
            credit,
            handoff,
            delivery,
        } = *evidence;
        self.update(operation, |record| {
            if delivery != uid {
                return record_split_settlement(record.swap.as_mut(), uid, evidence);
            }
            let order = record
                .swap
                .as_mut()
                .and_then(|swap| swap.orders.iter_mut().find(|order| order.uid == uid.0))
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            let (credit_fits, handoff_fits, delivered) = match order.delivery {
                SwapDelivery::Reshield => (
                    credit
                        .is_none_or(|credit| credit.private_amount >= order.bounds.private_minimum),
                    handoff.is_none(),
                    credit.is_some(),
                ),
                SwapDelivery::External { .. } => (credit.is_none(), handoff.is_none(), true),
                SwapDelivery::Bridge(bridge) => {
                    let handoff_fits = handoff.is_none_or(|handoff| {
                        handoff.observation == traded
                            && handoff.deposit_id.is_some()
                                == (bridge.provider == BridgeProvider::Across)
                    });
                    // Only an Across post-hook that reshields surplus shields anything, and
                    // only after its deposit. The surplus has no minimum.
                    let credit_fits = credit.is_none()
                        || bridge.surplus == BridgeSurplus::Reshield
                            && bridge.provider == BridgeProvider::Across
                            && handoff.is_some();
                    (credit_fits, handoff_fits, handoff.is_some())
                }
                // Like a NEAR Intents Bridge order: the trade pays the deposit address.
                SwapDelivery::NonEvmBridge(_) => (
                    credit.is_none(),
                    handoff.is_none_or(|handoff| {
                        handoff.observation == traded && handoff.deposit_id.is_none()
                    }),
                    handoff.is_some(),
                ),
            };
            if traded.transaction_hash.is_none()
                || !credit_fits
                || !handoff_fits
                || credit.is_some_and(|credit| credit.observation != traded)
                || order
                    .observations
                    .traded
                    .is_some_and(|known| known != traded)
                || handoff.is_some_and(|handoff| {
                    order
                        .observations
                        .bridge_handoff
                        .is_some_and(|known| known != handoff)
                })
            {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            let amounts = order.observations.kept_trade_amounts(amounts);
            order.observations.traded = Some(traded);
            order.observations.trade_amounts = Some(amounts);
            if let Some(credit) = credit {
                order.observations.settlement_credit = Some(credit);
            }
            if let Some(handoff) = handoff {
                order.observations.bridge_handoff = Some(handoff);
            }
            if delivered {
                order.observations.delivered = Some(traded);
                order.observations.undelivered = None;
            }
            Ok(())
        })
    }

    /// Persist the orderbook's executed fee with the amounts of an order's recorded trade. The
    /// fee is the orderbook's report, not chain evidence. A recorded fee is never replaced,
    /// though recording one again is accepted.
    pub(crate) fn record_swap_executed_fee(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        fee: U256,
        token: Address,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let order = record
                .swap
                .as_mut()
                .and_then(|swap| swap.orders.iter_mut().find(|order| order.uid == uid.0))
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            if order.observations.traded.is_none() {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            let amounts = order
                .observations
                .trade_amounts
                .as_mut()
                .ok_or(ExecutorStoreError::InvalidRecord)?;
            if amounts.executed_fee.is_none() {
                amounts.executed_fee = Some(fee);
                amounts.executed_fee_token = Some(token);
            }
            Ok(())
        })
    }

    /// Persist a Bridge order's destination outcome after its hand-off. A final outcome is
    /// never replaced, though recording it again is accepted; `NeedsAttention` may be
    /// replaced by any outcome. The exceptions are an Across `Refunding` without a verified
    /// refund, which a verified fill replaces, and an Across `DeliveredReported`, which a
    /// verified fill or a refund replaces. A verified delivery must meet the approved
    /// destination minimum. A private delivery's fill is recorded as a shielded delivery or as
    /// held on the destination chain, and no other delivery takes either.
    pub fn record_swap_bridge_outcome(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        outcome: SwapBridgeOutcome,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let order = record
                .swap
                .as_mut()
                .and_then(|swap| swap.orders.iter_mut().find(|order| order.uid == uid.0))
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            let private = match &order.delivery {
                SwapDelivery::Bridge(delivery) => delivery.is_private(),
                SwapDelivery::NonEvmBridge(_) => false,
                SwapDelivery::Reshield | SwapDelivery::External { .. } => {
                    return Err(ExecutorStoreError::OperationMismatch);
                }
            };
            let rules = BridgeOutcomeRules {
                handed_off: order.observations.bridge_handoff.is_some(),
                private,
                destination_minimum: order.bounds.destination_minimum,
                across: matches!(order.bridge, Some(BridgeOrderTerms::Across(_))),
                known: order.observations.bridge_outcome.clone(),
                refund_verified: order.observations.bridge_refund.is_some(),
            };
            if !rules.admits(&outcome) {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            order.observations.bridge_outcome = Some(outcome);
            Ok(())
        })
    }

    /// Persist Across's refund of a refunding order's deposit, in a finalized block after the
    /// hand-off. A recorded refund is never replaced, though recording it again is accepted.
    pub(crate) fn record_swap_bridge_refund(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        refund: SwapObservation,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let order = record
                .swap
                .as_mut()
                .and_then(|swap| swap.orders.iter_mut().find(|order| order.uid == uid.0))
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            let observed = &order.observations;
            if !bridge_refund_admitted(
                matches!(order.bridge, Some(BridgeOrderTerms::Across(_))),
                observed.bridge_outcome.as_ref(),
                observed.bridge_handoff,
                observed.bridge_refund,
                refund,
            ) {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            order.observations.bridge_refund = Some(refund);
            Ok(())
        })
    }
}
