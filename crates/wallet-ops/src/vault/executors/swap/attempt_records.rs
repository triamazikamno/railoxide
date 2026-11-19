#[allow(clippy::wildcard_imports)]
use super::*;

impl ExecutorStore {
    /// Refresh only mutable account state, retaining finalized swap and setup evidence, for
    /// an account settled in either role. A receipt credit does not identify a hook winner:
    /// require every old hook and shield nonce to be behind the freshly read nonce without
    /// inventing historical execution evidence. The nonce alone settles nothing: the record
    /// must already be settled at the observed block.
    pub(crate) fn refresh_settled_swap_nonce(
        &self,
        previous: &ExecutorRecord,
        observed: crate::vault::executors::ExecutorNonceObservation,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(previous.operation, |record| {
            if record != previous
                || !record.settled_swaps_at(observed.block.number)
                || record
                    .nonce_observation
                    .is_some_and(|old| observed.nonce < old.nonce)
                || record
                    .issued
                    .iter()
                    .any(|payload| payload.nonce >= observed.nonce)
            {
                return Err(ExecutorStoreError::OutstandingNonce);
            }
            record.nonce_observation = Some(observed);
            record.set_nonce_watermark(observed.into());
            Ok(())
        })
    }

    /// Persist a swap attempt's order and hook payloads in one write, before the
    /// order request exposes them. The pre-hook holds the current nonce `k`. A
    /// Reshield or Across Bridge order's post-hook holds `k + 1`; External and NEAR
    /// Intents Bridge orders have none. No other payload may use a future nonce. A
    /// Bridge order carries its provider's terms. A retry is admitted only after every
    /// earlier attempt ended or completed delivery. The order belongs to the attempt's swap
    /// use, and is refused once another use claims the account. Unless it is a private Bridge
    /// order, it keeps the tokens and delivery of the use's earlier attempts.
    ///
    /// A speed-up is the exception to the retry rule: it is admitted while the use's latest
    /// order is still open, at that order's pre-hook nonce. It must invalidate that order,
    /// keep the recorded proof, the sell amount, the private spend and the delivery, and ask
    /// for a lower buy amount. Earlier orders and the use's setup approval stay as they are.
    ///
    /// The pre-hook never shares a note with a setup whose nonce is unresolved, which could
    /// spend it first.
    pub fn record_swap_attempt(
        &self,
        operation: ExecutorOperationId,
        attempt: SwapAttempt,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.record_swap_attempt_at(operation, attempt, None)
    }

    /// [`Self::record_swap_attempt`] for an order, or its speed-up, on the included setup of
    /// the account its use reserved fresh: a read at the latest block showed the delegation
    /// and the account nonce `included_nonce`, while the record leaves the setup's nonce
    /// unresolved. The setup must sit unresolved at the record's observed nonce `n`, the
    /// pre-hook holds `n + 1`, which must be `included_nonce`, and a post-hook `n + 2`. Only
    /// that use's setups may be unresolved below the pre-hook. The hooks still name the
    /// record's confirmed observation, and nothing from the latest read is recorded.
    pub fn record_swap_attempt_on_included_setup(
        &self,
        operation: ExecutorOperationId,
        attempt: SwapAttempt,
        included_nonce: U256,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.record_swap_attempt_at(operation, attempt, Some(included_nonce))
    }

    fn record_swap_attempt_at(
        &self,
        operation: ExecutorOperationId,
        attempt: SwapAttempt,
        included_nonce: Option<U256>,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let SwapAttempt {
            use_id,
            terms,
            proof,
            uid,
            submission,
            delivery,
            bounds,
            invalidates,
            pre_hook,
            post_hook,
            bridge,
            speed_up,
        } = attempt;
        self.update(operation, |record| {
            if record.active_swap_use != Some(use_id) {
                return Err(ExecutorStoreError::SwapUseActive);
            }
            // The order is an attempt of the account's active use, which the latest use is. Only
            // a use that reserved its account fresh has a setup to be included.
            let Some(SwapUseRecord {
                stopped: false,
                fresh,
                role:
                    SwapUseRole::Source {
                        destination_operation,
                        ..
                    },
                ..
            }) = record
                .swap_uses
                .last()
                .filter(|swap_use| swap_use.id == use_id)
            else {
                return Err(ExecutorStoreError::OperationMismatch);
            };
            if included_nonce.is_some() && !*fresh {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            let destination_operation = *destination_operation;
            // A private Bridge order pays the destination account this use names, which must
            // serve this use for the delivery's token. Read under the held record lock.
            if let SwapDelivery::Bridge(bridge) = delivery
                && bridge.is_private()
            {
                let destination = match destination_operation {
                    Some(destination) => self
                        .for_chain(bridge.destination_chain)
                        .record(destination)?,
                    None => None,
                };
                if !destination.is_some_and(|destination| {
                    destination.address == Some(bridge.receiver)
                        && destination.swap_use(use_id).is_some_and(|swap_use| {
                            matches!(
                                swap_use.role,
                                SwapUseRole::Destination {
                                    origin_chain,
                                    origin_operation,
                                    destination_token,
                                    ..
                                } if origin_chain == self.chain_id
                                    && origin_operation == operation
                                    && destination_token == bridge.destination_token
                            )
                        })
                }) {
                    return Err(ExecutorStoreError::OperationMismatch);
                }
            }
            let orders = record
                .swap
                .as_ref()
                .map_or(&[][..], |swap| swap.orders.as_slice());
            if record.retired && record.swap.is_none()
                || record.swap_setup_stopped
                || record.address != Some(uid.owner())
                || record.public_account_uuid.is_some()
                || record
                    .swap
                    .as_ref()
                    .is_some_and(|swap| swap.terms.recipient != terms.recipient)
                || pre_hook.delegate != record.delegate
                || pre_hook.purpose != ExecutorPayloadPurpose::SwapPreHook
                || record
                    .issued
                    .iter()
                    .any(|issued| issued.hash == pre_hook.hash)
                || post_hook.is_some() != delivery.has_post_hook()
                || !bridge_terms_fit(&delivery, bridge.as_ref())
                || post_hook.as_ref().is_some_and(|post_hook| {
                    post_hook.delegate != record.delegate
                        || post_hook.purpose != ExecutorPayloadPurpose::SwapPostHook
                        || pre_hook.hash == post_hook.hash
                        || record
                            .issued
                            .iter()
                            .any(|issued| issued.hash == post_hook.hash)
                        || !post_hook.context.inputs.is_empty()
                })
                || proof.inputs.is_empty()
                || pre_hook.context.inputs != proof.inputs
                || orders.iter().any(|order| order.uid == uid.0)
                || invalidates.is_some_and(|old| orders.iter().all(|order| order.uid != old.0))
                || !record.issued.iter().any(|issued| {
                    issued.hash == terms.setup_payload
                        && issued.purpose == ExecutorPayloadPurpose::Operation
                })
            {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            if !record.swap_use_keeps_terms(use_id, terms.sell_token, terms.buy_token, &delivery) {
                return Err(ExecutorStoreError::SwapUseTermsChanged);
            }
            match speed_up {
                None => {
                    if !record
                        .swap
                        .as_ref()
                        .is_none_or(SwapOperationRecord::admits_attempt)
                    {
                        return Err(ExecutorStoreError::SwapAttemptOutstanding);
                    }
                }
                Some(open) => {
                    let binding = SwapSpeedUp {
                        swap_use: use_id,
                        order: open,
                        pre_hook_nonce: pre_hook.nonce,
                    };
                    // The rule leaves the open order as the latest one. Both orders fund from
                    // the same proof and sell the same amount, so only one of them can trade.
                    let admitted = record.swap.as_ref().is_some_and(|swap| {
                        swap.admits_speed_up(&binding)
                            && invalidates == Some(open)
                            && proof == swap.proof
                            && swap.orders.last().is_some_and(|latest| {
                                bounds.sell_amount == latest.bounds.sell_amount
                                    && bounds.spend_amount() == latest.bounds.spend_amount()
                                    && bounds.buy_amount < latest.bounds.buy_amount
                                    && delivery == latest.delivery
                            })
                    });
                    if !admitted {
                        return Err(ExecutorStoreError::SwapSpeedUpRefused);
                    }
                }
            }
            // This check shares the record mutation lock with every store handle.
            if self.records()?.iter().any(|other| {
                other.operation != operation
                    && other
                        .reserved_inputs()
                        .iter()
                        .any(|input| proof.inputs.contains(input))
            }) {
                return Err(ExecutorStoreError::InputReserved);
            }
            // A setup whose nonce is unresolved can still run before the pre-hook and spend
            // these notes as its fee, whichever attempt of it runs.
            if record.issued.iter().any(|issued| {
                issued.purpose == ExecutorPayloadPurpose::Operation
                    && !record.nonce_resolved(issued.nonce)
                    && issued
                        .context
                        .inputs
                        .iter()
                        .any(|input| proof.inputs.contains(input))
            }) {
                return Err(ExecutorStoreError::InputReserved);
            }
            let observed = pre_hook.context.observed;
            let nonce = pre_hook.nonce;
            // Without included evidence the setup's nonce must be resolved, whichever setup
            // signed at it ran. Every nonce below the current one is resolved with it, which
            // makes the old signatures of earlier hooks and shields unusable. On an included
            // setup the pre-hook takes the nonce after the setup's, and only the setup's
            // attempts may be unresolved below it.
            let setup_admitted = match included_nonce {
                None => {
                    observed.nonce == nonce
                        && record.issued.iter().any(|issued| {
                            issued.hash == terms.setup_payload
                                && record.nonce_resolved(issued.nonce)
                        })
                }
                Some(included) => {
                    record.unresolved_swap_setup_nonce(use_id) == Some(observed.nonce)
                        && observed.nonce.checked_add(U256::ONE) == Some(nonce)
                        && included == nonce
                        && record
                            .issued
                            .iter()
                            .find(|issued| issued.hash == terms.setup_payload)
                            .is_some_and(|setup| setup.nonce == observed.nonce)
                }
            };
            let included_setup_below = |issued: &IssuedExecutorPayload| {
                included_nonce.is_some()
                    && issued.purpose == ExecutorPayloadPurpose::Operation
                    && issued.nonce == observed.nonce
            };
            if record.nonce_observation != Some(observed)
                || !setup_admitted
                || pre_hook.context.calldata.is_empty()
                || post_hook.as_ref().is_some_and(|post_hook| {
                    post_hook.context.observed != observed
                        || nonce.checked_add(U256::ONE) != Some(post_hook.nonce)
                        || post_hook.context.calldata.is_empty()
                })
                || record.issued.iter().any(|issued| {
                    issued.nonce < nonce
                        && !record.nonce_resolved(issued.nonce)
                        && !included_setup_below(issued)
                        && !(issued.purpose == ExecutorPayloadPurpose::SwapPostHook
                            && (record.swap_post_hook_took_nonce(issued.nonce)
                                || record.swap.as_ref().is_some_and(|swap| {
                                    swap.orders.iter().any(|order| {
                                        order
                                            .post_hook
                                            .is_some_and(|hook| hook.payload == issued.hash)
                                            && swap.order_settled_at(order, observed.block.number)
                                    })
                                })))
                        && !(issued.purpose == ExecutorPayloadPurpose::SwapDestinationShield
                            && record.swap_shield_delivered(issued.hash))
                        && !orders
                            .iter()
                            .any(|order| order.pre_hook.payload == issued.hash)
                })
            {
                return Err(ExecutorStoreError::OutstandingNonce);
            }
            // A Reshield order pays the bought token to the executor. A Bridge order's bought
            // token can end up there too: a skipped Across post-hook leaves it, and a bridge
            // refunds to the executor. An External order pays its receiver. Earlier attempts'
            // assets stay. The wallet's native marker is not an ERC-20 asset.
            let bought = (matches!(delivery, SwapDelivery::Reshield) || delivery.is_bridge())
                .then_some(terms.buy_token);
            let order = SwapOrderRecord {
                terms: Some(terms),
                attempt: orders.last().map_or(0, |order| order.attempt + 1),
                uid: uid.0,
                delivery,
                bounds,
                pre_hook: SwapHookPayload::of(&pre_hook),
                post_hook: post_hook.as_ref().map(SwapHookPayload::of),
                invalidates: invalidates.map(|old| old.0),
                observations: SwapOrderObservations::default(),
                submission,
                submission_status: SwapSubmissionStatus::Pending,
                bridge,
                use_id: Some(use_id),
            };
            if let Some(swap) = &mut record.swap {
                swap.proof = proof;
                swap.orders.push(order);
            } else {
                record.swap = Some(SwapOperationRecord {
                    terms,
                    proof,
                    orders: vec![order],
                    fundings: Vec::new(),
                });
            }
            record.issued.push(pre_hook);
            record.issued.extend(post_hook);
            for token in std::iter::once(terms.sell_token).chain(bought) {
                if token == Address::ZERO {
                    continue;
                }
                let asset = crate::vault::executors::ExecutorAsset::Erc20(token);
                if !record.assets.contains(&asset) {
                    record.assets.push(asset);
                }
            }
            record.hidden = false;
            Ok(())
        })
    }
}
