#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn update_private_broadcaster_progress_stage(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        stage: TransactionGenerationStage,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return false;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return false;
        }
        let should_open_dialog = !progress.stage_seen && !progress.dialog_open;
        progress.stage_seen = true;
        if progress.flow == PrivateSubmissionProgressFlow::PublicBroadcaster
            && stage == TransactionGenerationStage::WaitingForBroadcasterResponse
            && progress.public_broadcaster_wait_started_at.is_none()
        {
            progress.public_broadcaster_wait_started_at = Some(Instant::now());
        }
        if private_progress_stage_disables_stop(progress.flow, progress.sponsored_funding, stage) {
            progress.stop_available = false;
        }
        ensure_private_broadcaster_progress_stage(progress, stage);
        apply_private_broadcaster_progress_stage(&mut progress.steps, stage);
        cx.notify();
        should_open_dialog
    }

    pub(in crate::root) fn set_private_self_broadcast_unshield_poi_step(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        required: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id)
            || progress.flow != PrivateSubmissionProgressFlow::SelfBroadcast
            || progress.kind != DeliveryFormKind::Unshield
        {
            return;
        }
        if required {
            ensure_self_broadcast_unshield_progress_stage(
                &mut progress.steps,
                TransactionGenerationStage::GeneratingPoiProofs,
            );
        } else if let Some(index) = progress.steps.iter().position(|step| {
            step.stage == TransactionGenerationStage::GeneratingPoiProofs
                && step.status == PublicActionStepStatus::NotStarted
        }) {
            progress.steps.remove(index);
        }
        cx.notify();
    }

    pub(in crate::root) fn finish_private_broadcaster_progress(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        final_stage: TransactionGenerationStage,
        result: PublicBroadcasterSubmissionResult,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        finish_private_broadcaster_progress_steps_at_stage(
            &mut progress.steps,
            final_stage,
            &result.result,
        );
        progress.task_abort_handle = None;
        progress.payload_inputs_rx = None;
        progress.stop_available = false;
        progress.result = Some(result);
        progress.error = None;
        cx.notify();
    }

    pub(in crate::root) fn finish_private_self_broadcast_progress(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        final_stage: TransactionGenerationStage,
        result: DesktopSelfBroadcastResult,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        ensure_private_broadcaster_progress_stage(progress, final_stage);
        finish_private_self_broadcast_progress_steps_at_stage(
            &mut progress.steps,
            final_stage,
            result.tx.execution_status(),
        );
        if result.tx.receipt().is_some()
            && result.tx.execution_status().is_none()
            && let Some(step) = progress.steps.last_mut()
        {
            step.message = Some(Arc::from(
                "The execution is not confirmed yet. This operation remains available in Stealth accounts.",
            ));
        }
        progress
            .self_broadcast_attempts
            .clone_from(&result.attempts);
        progress.self_broadcast_current_gas_fee =
            Some((result.max_fee_per_gas, result.max_priority_fee_per_gas));
        progress.self_broadcast_action_error = None;
        progress.self_broadcast_result = Some(result);
        progress.self_broadcast_command_tx = None;
        progress.task_abort_handle = None;
        progress.stop_available = false;
        progress.error = None;
        cx.notify();
    }

    pub(in crate::root) fn finish_private_sponsored_self_broadcast_progress(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        final_stage: TransactionGenerationStage,
        outcome: SponsoredSelfBroadcastSessionOutcome,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        progress.sponsored_self_broadcast_outcome = Some(outcome.clone());
        match outcome {
            SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(receipt) => {
                ensure_private_broadcaster_progress_stage(progress, final_stage);
                finish_private_self_broadcast_progress_steps_at_stage(
                    &mut progress.steps,
                    final_stage,
                    Some(receipt.status),
                );
            }
            SponsoredSelfBroadcastSessionOutcome::Stopped { .. } => {
                mark_private_broadcaster_active_step_stopped(&mut progress.steps);
                progress.stopped = true;
            }
        }
        progress.sponsored_self_broadcast_command_tx = None;
        progress.task_abort_handle = None;
        progress.stop_available = false;
        progress.error = None;
        cx.notify();
    }

    pub(in crate::root) fn fail_private_broadcaster_progress(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        final_stage: TransactionGenerationStage,
        message: String,
        input_pin_conflict: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        ensure_private_broadcaster_progress_stage(progress, final_stage);
        fail_private_broadcaster_progress_steps_at_stage(
            &mut progress.steps,
            final_stage,
            message.as_str(),
        );
        progress.self_broadcast_action_error = None;
        progress.input_pin_conflict = input_pin_conflict;
        progress.error = Some(Arc::from(message));
        progress.self_broadcast_command_tx = None;
        progress.task_abort_handle = None;
        progress.payload_inputs_rx = None;
        progress.stop_available = false;
        cx.notify();
    }

    pub(in crate::root) fn record_private_broadcaster_progress_step_error(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        stage: TransactionGenerationStage,
        message: &str,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return false;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return false;
        }
        progress.stage_seen = true;
        ensure_private_broadcaster_progress_stage(progress, stage);
        fail_private_broadcaster_progress_steps_at_stage(&mut progress.steps, stage, message);
        progress.self_broadcast_action_error = None;
        cx.notify();
        !progress.dialog_open
    }

    pub(in crate::root) fn record_private_self_broadcast_attempt(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        attempt: SelfBroadcastAttemptInfo,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        progress.self_broadcast_current_gas_fee =
            Some((attempt.max_fee_per_gas, attempt.max_priority_fee_per_gas));
        progress.self_broadcast_action_error = None;
        progress.stop_available = false;
        progress.self_broadcast_attempts.push(attempt);
        cx.notify();
    }

    pub(in crate::root) fn record_private_self_broadcast_attempt_rejected(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        message: String,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        progress.self_broadcast_action_error = Some(Arc::from(message));
        cx.notify();
    }
}
