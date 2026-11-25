use super::*;
use ui::controls::AppNoticeTone;
use wallet_ops::gateway::GatewayDraftExecution;
use zeroize::Zeroizing;

/// A pinned form reached a path off its pinned notes without the user accepting the risk.
const PINNED_FORM_LEFT_BROADCASTER: &str = "An earlier proof for this payment may still go through. Submit again to choose whether to continue.";

const SOFTWARE_SELF_BROADCAST_GAS_PAYER_PASSWORD_REQUIRED: &str = concat!(
    "The selected gas payer is a software Public account, so self-broadcast requires the vault ",
    "password. Choose a hardware Public account, manual calldata, or public broadcaster delivery."
);

fn gateway_private_estimate_is_ready(
    execution: Option<&GatewayDraftExecution>,
    estimate_is_current: bool,
    estimated_at: Option<std::time::Instant>,
    age_limited: bool,
) -> bool {
    execution.is_none_or(|execution| {
        // Quote age gates entry into approval. Execution recalculates fees after approval,
        // while an invalidated estimate must still stop submission.
        estimate_is_current
            && (!age_limited
                || execution.has_review_approval()
                || estimated_at.is_some_and(|at| at.elapsed() <= Duration::from_secs(30)))
    })
}

/// Invalidating re-estimates clear the shown estimate, so one still shown during a refresh is
/// the retained background kind and stays submittable; the review bounds any newer quote.
pub(in crate::root) const fn broadcaster_estimate_allows_submission(
    estimate: Option<&PublicBroadcasterCostEstimate>,
    _refreshing: bool,
) -> bool {
    estimate.is_some()
}

/// Submitting is how the user reaches the pin override confirmation after a pin conflict.
pub(in crate::root) fn broadcaster_input_pin_conflict_awaits_choice(
    input_pin: Option<&wallet_ops::PublicBroadcasterInputPin>,
    conflict: bool,
    refreshing: bool,
) -> bool {
    conflict && !refreshing && input_pin.is_some_and(|pin| !pin.overridden)
}

pub(in crate::root) fn sponsored_authorization_display(
    chain_id: u64,
    limit: SponsoredAuthorizationLimit,
    token_registry: &EffectiveTokenRegistry,
    anchor_cache: &TokenAnchorRateCache,
) -> SponsoredAuthorizationDisplay {
    let maximum_payment = limit
        .maximum_payment()
        .expect("sponsored authorization limit was validated when quoted");
    let amount_label = |amount| {
        let token_value = format_token_amount_ceiling_for_display(
            chain_id,
            limit.wrapped_native_token,
            amount,
            Some(token_registry),
        );
        let usd_micro_value =
            anchor_cache.cached_token_usd_micro_value(chain_id, limit.wrapped_native_token, amount);
        format!(
            "Up to {}",
            format_value_with_usd_label(
                token_value,
                amount,
                token_display_metadata(
                    Some(token_registry),
                    chain_id,
                    &limit.wrapped_native_token,
                )
                .map(|metadata| metadata.decimals),
                usd_micro_value,
                false,
            )
        )
    };
    SponsoredAuthorizationDisplay {
        gross_wrapped_native_spend: amount_label(maximum_payment.gross_wrapped_native_spend),
        max_fee_per_gas: format!("{} gwei", format_gwei(limit.max_fee_per_gas)),
        max_priority_fee_per_gas: format!("{} gwei", format_gwei(limit.max_priority_fee_per_gas)),
    }
}

/// Shows the automatic broadcaster fee and the ceiling submission enforces for it.
fn with_broadcaster_fee_rows(
    summary: SpendAuthorizationSummary,
    chain_id: u64,
    review: &PrivateFeeReview,
    fee_mode: FeeHandlingMode,
    token_registry: &EffectiveTokenRegistry,
) -> SpendAuthorizationSummary {
    let label = |amount| {
        super::fee_editor::custom_fee_label(
            chain_id,
            review.quote.fee_token,
            amount,
            token_registry,
        )
    };
    summary.with_broadcaster_fee(
        label(review.quote.fee_amount),
        match fee_mode {
            FeeHandlingMode::DeductFromAmount => "Maximum transaction fee (deducted from amount)",
            FeeHandlingMode::AddToAmount => "Maximum transaction fee (added to amount)",
        },
        label(review.bounds.maximum_fee()),
    )
}

const INPUT_PIN_CONFLICT_MESSAGE: &str =
    "This retry would spend different notes than the earlier attempt, so nothing was sent.";

/// A failed submission's message, and whether it stopped on the form's note pin.
fn submission_error_message(error: &eyre::Report) -> (String, bool) {
    if error
        .downcast_ref::<wallet_ops::PublicBroadcasterInputPinUnavailable>()
        .is_some()
    {
        (INPUT_PIN_CONFLICT_MESSAGE.to_owned(), true)
    } else {
        (format_report_chain(error), false)
    }
}

/// The form's note pin after a published attempt.
fn input_pin_after_attempt(
    pin: Option<wallet_ops::PublicBroadcasterInputPin>,
    result: &PublicBroadcasterSubmissionResult,
) -> Option<wallet_ops::PublicBroadcasterInputPin> {
    let may_still_execute = matches!(
        result.result,
        PublicBroadcasterResultKind::Failed { .. } | PublicBroadcasterResultKind::TimedOut
    );
    input_pin_transition(pin, may_still_execute, result.payload_inputs.as_deref())
}

/// A refused, unanswered or abandoned attempt with one payload transaction narrows the pin
/// to the notes it shares with that transaction, so a later attempt conflicts with both
/// proofs. Without shared notes, such as after an override, it pins that transaction's
/// notes. An override covered only that attempt.
fn input_pin_transition(
    pin: Option<wallet_ops::PublicBroadcasterInputPin>,
    may_still_execute: bool,
    payload_inputs: Option<&[wallet_ops::SelectedInputIdentity]>,
) -> Option<wallet_ops::PublicBroadcasterInputPin> {
    match payload_inputs {
        Some(inputs) if may_still_execute && !inputs.is_empty() => {
            let shared = pin
                .map(|pin| {
                    pin.inputs
                        .into_iter()
                        .filter(|input| inputs.contains(input))
                        .collect::<Vec<_>>()
                })
                .filter(|shared| !shared.is_empty());
            Some(wallet_ops::PublicBroadcasterInputPin {
                inputs: shared.unwrap_or_else(|| inputs.to_vec()),
                overridden: false,
            })
        }
        _ => pin.map(|pin| wallet_ops::PublicBroadcasterInputPin {
            overridden: false,
            ..pin
        }),
    }
}

/// Review text for a batch of several plain transactions, which a broadcaster can split.
/// It informs only; the note pin never depends on this estimate.
fn unbundling_disclosure(
    estimate: &PublicBroadcasterCostEstimate,
    action_symbol: &str,
) -> Option<String> {
    if estimate.transaction_count < 2 || estimate.uses_relay_adapt {
        return None;
    }
    // Same-token fees ride in the first proof, so a payment that fits one proof is atomic.
    Some(if estimate.fee_token == estimate.action_token {
        "Your private balance is spread across many smaller amounts, so a malicious broadcaster could collect the fee without delivering part of this payment. Sending a smaller amount at a time avoids this; a trusted broadcaster reduces the risk.".to_owned()
    } else {
        format!(
            "A malicious broadcaster could collect the fee without delivering some or all of this payment. Using a trusted broadcaster or paying the fee in {action_symbol} reduces this risk."
        )
    })
}

/// The broadcaster fee review for a draft; it pins the shown broadcaster for custom fees too.
fn broadcaster_fee_review(
    delivery_mode: DeliveryMode,
    custom_fee_amount: Option<U256>,
    cost_estimate: Option<&PublicBroadcasterCostEstimate>,
) -> eyre::Result<Option<Arc<PrivateFeeReview>>> {
    cost_estimate
        .filter(|_| delivery_mode == DeliveryMode::PublicBroadcaster)
        .map(|estimate| PrivateFeeReview::new(estimate.clone(), custom_fee_amount).map(Arc::new))
        .transpose()
}

impl WalletRoot {
    /// Before the running public broadcaster attempt is abandoned, pins its form to the
    /// payload it already sent, as an unanswered attempt would. Taking the receiver applies
    /// it at most once.
    pub(in crate::root) fn pin_abandoned_private_broadcaster_payload(&mut self) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        let Some(Ok(inputs)) = progress
            .payload_inputs_rx
            .take()
            .map(|mut payload_inputs_rx| payload_inputs_rx.try_recv())
        else {
            return;
        };
        let (kind, key, generation_id) = (progress.kind, progress.key, progress.generation_id);
        let pin = match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get_mut(&key)
                .filter(|form| form.generation_id == generation_id)
                .map(|form| &mut form.input_pin),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get_mut(&key)
                .filter(|form| form.generation_id == generation_id)
                .map(|form| &mut form.input_pin),
        };
        if let Some(pin) = pin {
            *pin = input_pin_transition(pin.take(), true, Some(inputs.as_slice()));
        }
    }

    pub(in crate::root) fn watch_self_broadcast_session_events(
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        mut event_rx: mpsc::UnboundedReceiver<SelfBroadcastSessionEvent>,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = event_rx.recv().await {
                let _ = this.update_in(cx, |root, window, cx| match event {
                    SelfBroadcastSessionEvent::PendingOutputPoiProofsRequired { required } => {
                        root.set_private_self_broadcast_unshield_poi_step(
                            kind,
                            key,
                            generation_id,
                            required,
                            cx,
                        );
                    }
                    SelfBroadcastSessionEvent::StepFailed { stage, message } => {
                        if root.record_private_broadcaster_progress_step_error(
                            kind,
                            key,
                            generation_id,
                            stage,
                            &message,
                            cx,
                        ) {
                            root.show_private_broadcaster_progress_dialog(window, cx);
                        }
                    }
                    SelfBroadcastSessionEvent::AttemptSubmitted(attempt) => {
                        root.record_private_self_broadcast_attempt(
                            kind,
                            key,
                            generation_id,
                            attempt,
                            cx,
                        );
                    }
                    SelfBroadcastSessionEvent::AttemptRejected { message, .. } => {
                        root.record_private_self_broadcast_attempt_rejected(
                            kind,
                            key,
                            generation_id,
                            message,
                            cx,
                        );
                    }
                    SelfBroadcastSessionEvent::HardwareProfileSessionRefreshed { session } => {
                        #[cfg(feature = "hardware")]
                        root.refresh_active_hardware_profile_session(session, cx);
                        #[cfg(not(feature = "hardware"))]
                        let _ = session;
                    }
                });
            }
        })
        .detach();
    }
}

mod send_generation;
mod unshield_generation;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_quote_age_does_not_expire_an_approved_spend() {
        let execution = GatewayDraftExecution::private("approval-expiry-test".into());
        let estimated_at = std::time::Instant::now();
        assert!(gateway_private_estimate_is_ready(
            Some(&execution),
            true,
            Some(estimated_at),
            true,
        ));
        let expired_at = estimated_at
            .checked_sub(Duration::from_mins(1))
            .expect("earlier quote timestamp");
        assert!(!gateway_private_estimate_is_ready(
            Some(&execution),
            true,
            Some(expired_at),
            true,
        ));

        assert!(gateway_private_estimate_is_ready(
            Some(&execution),
            true,
            Some(expired_at),
            false
        ));
        assert!(!gateway_private_estimate_is_ready(
            Some(&execution),
            false,
            Some(expired_at),
            false
        ));
        assert!(execution.approve_review());
        assert!(gateway_private_estimate_is_ready(
            Some(&execution),
            true,
            Some(expired_at),
            true,
        ));
        assert!(!gateway_private_estimate_is_ready(
            Some(&execution),
            false,
            Some(expired_at),
            true,
        ));
    }

    #[test]
    fn refused_or_unanswered_single_payload_attempts_narrow_the_note_pin() {
        use wallet_ops::{PublicBroadcasterInputPin, SelectedInputIdentity};
        let broadcaster = wallet_ops::public_broadcaster_candidates_for_asset(
            &[crate::root::tests::fee_row(1, Address::ZERO, "pin")],
            1,
            Address::ZERO,
            BroadcasterFeePolicy::default(),
            None,
        )
        .unwrap()
        .remove(0);
        let attempt = |result, payload_inputs| PublicBroadcasterSubmissionResult {
            broadcaster: broadcaster.clone(),
            action_token: Address::ZERO,
            fee_token: Address::ZERO,
            entered_amount: U256::from(100),
            receiver_amount: U256::from(99),
            recipient_amount: U256::from(99),
            total_private_spend: U256::from(100),
            fee_amount: U256::ONE,
            protocol_fee_amount: U256::ZERO,
            protocol_fee_bps: U256::ZERO,
            fee_mode: FeeHandlingMode::DeductFromAmount,
            gas_limit: 1,
            min_gas_price: 1,
            transaction_count: 1,
            input_count: 1,
            private_output_count: 1,
            public_output_count: 0,
            relay_call_count: 0,
            uses_relay_adapt: false,
            result,
            native_top_up: None,
            payload_inputs,
        };
        let note = |position| SelectedInputIdentity { tree: 0, position };
        let pin = |positions: &[u64], overridden| PublicBroadcasterInputPin {
            inputs: positions.iter().copied().map(note).collect(),
            overridden,
        };
        let refused = || PublicBroadcasterResultKind::Failed {
            error: "refused".into(),
        };
        for outcome in [refused(), PublicBroadcasterResultKind::TimedOut] {
            assert_eq!(
                input_pin_after_attempt(None, &attempt(outcome, Some(vec![note(1), note(2)]))),
                Some(pin(&[1, 2], false))
            );
        }
        // A payload of several transactions can't be pinned and installs nothing.
        assert_eq!(
            input_pin_after_attempt(None, &attempt(refused(), None)),
            None
        );
        // A pinned retry shares some of the pinned notes; only those conflict with both proofs.
        assert_eq!(
            input_pin_after_attempt(
                Some(pin(&[1, 2], false)),
                &attempt(PublicBroadcasterResultKind::TimedOut, Some(vec![note(1)])),
            ),
            Some(pin(&[1], false))
        );
        assert_eq!(
            input_pin_after_attempt(
                Some(pin(&[1, 2], false)),
                &attempt(refused(), Some(vec![note(2), note(3)])),
            ),
            Some(pin(&[2], false))
        );
        // An overridden attempt that shared no note replaces the pin with its own notes.
        assert_eq!(
            input_pin_after_attempt(
                Some(pin(&[1, 2], true)),
                &attempt(refused(), Some(vec![note(3)])),
            ),
            Some(pin(&[3], false))
        );
        // Otherwise the pin stays, and an override covered only the attempt it was given for.
        for (outcome, payload) in [
            (refused(), None),
            (
                PublicBroadcasterResultKind::Submitted {
                    tx_hash: alloy::primitives::B256::repeat_byte(0xab),
                },
                Some(vec![note(3)]),
            ),
        ] {
            assert_eq!(
                input_pin_after_attempt(Some(pin(&[1, 2], true)), &attempt(outcome, payload)),
                Some(pin(&[1, 2], false))
            );
        }
    }
}
