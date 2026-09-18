use std::sync::Arc;
use std::time::{Duration, Instant};

use wallet_ops::{
    PublicBroadcasterResultKind, SponsoredSelfBroadcastSessionOutcome, TransactionGenerationStage,
};

use super::types::{
    PRIVATE_BROADCASTER_PROGRESS_STAGES, PUBLIC_BROADCASTER_STOP_RESEND_THRESHOLD,
    PrivateBroadcasterClosedActiveProgress, PrivateBroadcasterProgressState,
    PrivateBroadcasterProgressStepState, PrivateSubmissionProgressFlow,
    SELF_BROADCAST_PROGRESS_STAGES, SELF_BROADCAST_UNSHIELD_PROGRESS_STAGES,
    SelfBroadcastGasRetryKind,
};
use crate::root::public_action::{
    ProgressDialogCloseBehavior, ProgressFooterAction, PublicActionStepStatus,
    progress_dialog_close_behavior, progress_footer_action,
};
use crate::root::{DeliveryFormKind, UnshieldAssetKey};

pub(in crate::root) fn private_broadcaster_progress_steps()
-> Vec<PrivateBroadcasterProgressStepState> {
    progress_steps(&PRIVATE_BROADCASTER_PROGRESS_STAGES)
}

pub(in crate::root) fn self_broadcast_progress_steps(
    kind: DeliveryFormKind,
) -> Vec<PrivateBroadcasterProgressStepState> {
    match kind {
        DeliveryFormKind::Send => progress_steps(&SELF_BROADCAST_PROGRESS_STAGES),
        DeliveryFormKind::Unshield => progress_steps(&SELF_BROADCAST_UNSHIELD_PROGRESS_STAGES),
    }
}

pub(in crate::root) fn ensure_self_broadcast_unshield_progress_stage(
    steps: &mut Vec<PrivateBroadcasterProgressStepState>,
    stage: TransactionGenerationStage,
) {
    if stage != TransactionGenerationStage::GeneratingPoiProofs
        || steps.iter().any(|step| step.stage == stage)
    {
        return;
    }
    let insert_index = steps
        .iter()
        .position(|step| step.stage == TransactionGenerationStage::EstimatingSelfBroadcastGas)
        .unwrap_or(steps.len());
    let status = if steps
        .get(insert_index)
        .is_some_and(|step| step.status != PublicActionStepStatus::NotStarted)
    {
        PublicActionStepStatus::Done
    } else {
        PublicActionStepStatus::NotStarted
    };
    steps.insert(
        insert_index,
        PrivateBroadcasterProgressStepState {
            stage,
            status,
            message: None,
        },
    );
}

pub(super) fn ensure_private_broadcaster_progress_stage(
    progress: &mut PrivateBroadcasterProgressState,
    stage: TransactionGenerationStage,
) {
    if progress.flow == PrivateSubmissionProgressFlow::SelfBroadcast
        && progress.kind == DeliveryFormKind::Unshield
    {
        ensure_self_broadcast_unshield_progress_stage(&mut progress.steps, stage);
    }
}

fn progress_steps(
    stages: &[TransactionGenerationStage],
) -> Vec<PrivateBroadcasterProgressStepState> {
    stages
        .iter()
        .enumerate()
        .map(|(index, &stage)| PrivateBroadcasterProgressStepState {
            stage,
            status: if index == 0 {
                PublicActionStepStatus::Pending
            } else {
                PublicActionStepStatus::NotStarted
            },
            message: None,
        })
        .collect()
}

pub(in crate::root) fn private_broadcaster_closed_active_progress(
    progress: Option<&PrivateBroadcasterProgressState>,
    kind: DeliveryFormKind,
    key: UnshieldAssetKey,
    generation_id: u64,
) -> Option<PrivateBroadcasterClosedActiveProgress> {
    let progress = progress?;
    if !progress.accepts_update(kind, key, generation_id)
        || progress.dialog_open
        || !progress.stage_seen
    {
        return None;
    }
    let step = progress
        .steps
        .iter()
        .find(|step| step.status == PublicActionStepStatus::Pending)
        .or_else(|| {
            progress
                .steps
                .iter()
                .find(|step| step.status == PublicActionStepStatus::Error)
        })?;
    Some(PrivateBroadcasterClosedActiveProgress {
        flow: progress.flow,
        generation_id: progress.generation_id,
        requires_device_approval: progress.requires_device_approval,
        step: step.clone(),
    })
}

#[cfg(test)]
pub(in crate::root) fn private_broadcaster_closed_active_stage(
    progress: Option<&PrivateBroadcasterProgressState>,
    kind: DeliveryFormKind,
    key: UnshieldAssetKey,
    generation_id: u64,
) -> Option<TransactionGenerationStage> {
    private_broadcaster_closed_active_progress(progress, kind, key, generation_id)
        .map(|active| active.step.stage)
}

pub(in crate::root) const fn private_submission_discard_attempt_available(
    active: &PrivateBroadcasterClosedActiveProgress,
) -> bool {
    matches!(active.step.status, PublicActionStepStatus::Error)
}

pub(in crate::root) const fn private_progress_stage_disables_stop(
    flow: PrivateSubmissionProgressFlow,
    sponsored_funding: bool,
    stage: TransactionGenerationStage,
) -> bool {
    match flow {
        PrivateSubmissionProgressFlow::PublicBroadcaster => matches!(
            stage,
            TransactionGenerationStage::PublishingToBroadcaster
                | TransactionGenerationStage::WaitingForBroadcasterResponse
        ),
        PrivateSubmissionProgressFlow::SelfBroadcast => {
            !sponsored_funding
                && matches!(
                    stage,
                    TransactionGenerationStage::SigningSelfBroadcast
                        | TransactionGenerationStage::WaitingForSelfBroadcastReceipt
                )
        }
    }
}

pub(in crate::root) const fn sponsored_stop_uses_session_command(
    stage: TransactionGenerationStage,
) -> bool {
    matches!(
        stage,
        TransactionGenerationStage::SigningSelfBroadcast
            | TransactionGenerationStage::WaitingForSelfBroadcastReceipt
    )
}

pub(in crate::root) fn private_broadcaster_progress_footer_action(
    progress: &PrivateBroadcasterProgressState,
) -> ProgressFooterAction {
    progress_footer_action(
        private_broadcaster_progress_stop_available(progress, Instant::now()),
        private_broadcaster_progress_is_terminal(progress),
    )
}

pub(in crate::root) fn private_broadcaster_progress_dialog_close_behavior(
    progress: &PrivateBroadcasterProgressState,
) -> ProgressDialogCloseBehavior {
    if progress.gateway_execution.is_some() && private_self_broadcast_requires_attention(progress) {
        return ProgressDialogCloseBehavior::TopOnly;
    }
    if progress.gateway_execution.is_some() && private_broadcaster_progress_is_terminal(progress) {
        // Extension submissions have no desktop form to return to after closing their result.
        return ProgressDialogCloseBehavior::AllAndClear;
    }
    progress_dialog_close_behavior(
        private_broadcaster_progress_is_successful(progress),
        progress.stopped,
    )
}

pub(super) fn private_self_broadcast_requires_attention(
    progress: &PrivateBroadcasterProgressState,
) -> bool {
    progress.flow == PrivateSubmissionProgressFlow::SelfBroadcast
        && progress.self_broadcast_command_tx.is_some()
        && progress.self_broadcast_result.is_none()
        && progress.error.is_none()
        && !progress.stopped
        && (progress.self_broadcast_action_error.is_some()
            || progress.steps.iter().any(|step| {
                step.status == PublicActionStepStatus::Error
                    && self_broadcast_step_retry_kind(progress, step).is_some()
            }))
}

pub(super) fn private_broadcaster_progress_stop_available(
    progress: &PrivateBroadcasterProgressState,
    now: Instant,
) -> bool {
    progress.stop_available || public_broadcaster_waiting_can_stop(progress, now)
}

pub(super) fn public_broadcaster_waiting_can_stop(
    progress: &PrivateBroadcasterProgressState,
    now: Instant,
) -> bool {
    public_broadcaster_resend_count(progress, now)
        .is_some_and(|count| count >= PUBLIC_BROADCASTER_STOP_RESEND_THRESHOLD)
}

pub(in crate::root) fn private_broadcaster_progress_is_terminal(
    progress: &PrivateBroadcasterProgressState,
) -> bool {
    progress.stopped
        || progress.result.is_some()
        || progress.self_broadcast_result.is_some()
        || progress.sponsored_self_broadcast_outcome.is_some()
        || progress.error.is_some()
        || (!progress.steps.is_empty()
            && (progress
                .steps
                .iter()
                .all(|step| step.status == PublicActionStepStatus::Done)
                || progress.steps.iter().any(|step| {
                    matches!(
                        step.status,
                        PublicActionStepStatus::Error | PublicActionStepStatus::Stopped
                    )
                })))
}

pub(in crate::root) fn private_broadcaster_progress_is_successful(
    progress: &PrivateBroadcasterProgressState,
) -> bool {
    match progress.flow {
        PrivateSubmissionProgressFlow::PublicBroadcaster => {
            progress.result.as_ref().is_some_and(|result| {
                matches!(result.result, PublicBroadcasterResultKind::Submitted { .. })
            })
        }
        PrivateSubmissionProgressFlow::SelfBroadcast => {
            progress
                .self_broadcast_result
                .as_ref()
                .is_some_and(|result| result.tx.execution_status() == Some(true))
                || progress
                    .sponsored_self_broadcast_outcome
                    .as_ref()
                    .is_some_and(|outcome| {
                        matches!(
                            outcome,
                            SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(receipt)
                                if receipt.status
                        )
                    })
        }
    }
}

pub(in crate::root) fn mark_private_broadcaster_active_step_stopped(
    steps: &mut [PrivateBroadcasterProgressStepState],
) -> bool {
    let step_index = steps
        .iter()
        .position(|step| step.status == PublicActionStepStatus::Pending)
        .or_else(|| {
            steps
                .iter()
                .position(|step| step.status == PublicActionStepStatus::Error)
        })
        .or_else(|| {
            steps
                .iter()
                .rposition(|step| step.status == PublicActionStepStatus::NotStarted)
        });
    let Some(step_index) = step_index else {
        return false;
    };
    let step = &mut steps[step_index];
    step.status = PublicActionStepStatus::Stopped;
    step.message = None;
    true
}

pub(in crate::root) fn apply_private_broadcaster_progress_stage(
    steps: &mut [PrivateBroadcasterProgressStepState],
    stage: TransactionGenerationStage,
) {
    let active_index = private_progress_stage_index(steps, stage);
    for (index, step) in steps.iter_mut().enumerate() {
        step.status = match index.cmp(&active_index) {
            std::cmp::Ordering::Less => PublicActionStepStatus::Done,
            std::cmp::Ordering::Equal => PublicActionStepStatus::Pending,
            std::cmp::Ordering::Greater => PublicActionStepStatus::NotStarted,
        };
        step.message = None;
    }
}

pub(super) const SELF_BROADCAST_INCLUSION_UNOBSERVED_MESSAGE: &str =
    "Unable to observe transaction inclusion. Check the transaction hash before sending again.";

pub(in crate::root) fn finish_private_self_broadcast_progress_steps(
    steps: &mut [PrivateBroadcasterProgressStepState],
    receipt_status: Option<bool>,
) {
    if receipt_status.is_none() {
        for step in steps {
            if step.stage == TransactionGenerationStage::WaitingForSelfBroadcastReceipt {
                step.status = PublicActionStepStatus::Warning;
                step.message = Some(Arc::from(SELF_BROADCAST_INCLUSION_UNOBSERVED_MESSAGE));
            } else {
                step.status = PublicActionStepStatus::Done;
                step.message = None;
            }
        }
    } else if receipt_status == Some(true) {
        for step in steps {
            step.status = PublicActionStepStatus::Done;
            step.message = None;
        }
    } else {
        fail_private_broadcaster_progress_steps(
            steps,
            "Transaction receipt indicates the self-broadcast transaction reverted.",
        );
    }
}

pub(in crate::root) fn finish_private_broadcaster_progress_steps(
    steps: &mut [PrivateBroadcasterProgressStepState],
    result: &PublicBroadcasterResultKind,
) {
    match result {
        PublicBroadcasterResultKind::Submitted { .. } => {
            for step in steps {
                step.status = PublicActionStepStatus::Done;
                step.message = None;
            }
        }
        PublicBroadcasterResultKind::Failed { error } => {
            let message = format!("Broadcaster returned an error: {error}");
            fail_private_broadcaster_progress_steps(steps, &message);
        }
        PublicBroadcasterResultKind::TimedOut => fail_private_broadcaster_progress_steps(
            steps,
            "No decryptable broadcaster response arrived before the timeout.",
        ),
    }
}

pub(in crate::root) fn finish_private_broadcaster_progress_steps_at_stage(
    steps: &mut [PrivateBroadcasterProgressStepState],
    final_stage: TransactionGenerationStage,
    result: &PublicBroadcasterResultKind,
) {
    apply_private_broadcaster_progress_stage(steps, final_stage);
    finish_private_broadcaster_progress_steps(steps, result);
}

pub(in crate::root) fn finish_private_self_broadcast_progress_steps_at_stage(
    steps: &mut [PrivateBroadcasterProgressStepState],
    final_stage: TransactionGenerationStage,
    receipt_status: Option<bool>,
) {
    apply_private_broadcaster_progress_stage(steps, final_stage);
    finish_private_self_broadcast_progress_steps(steps, receipt_status);
}

pub(in crate::root) fn fail_private_broadcaster_progress_steps_at_stage(
    steps: &mut [PrivateBroadcasterProgressStepState],
    final_stage: TransactionGenerationStage,
    message: &str,
) {
    apply_private_broadcaster_progress_stage(steps, final_stage);
    fail_private_broadcaster_progress_steps(steps, message);
}

fn fail_private_broadcaster_progress_steps(
    steps: &mut [PrivateBroadcasterProgressStepState],
    message: &str,
) {
    let message = Arc::<str>::from(message);
    let error_index = steps
        .iter()
        .position(|step| step.status == PublicActionStepStatus::Pending)
        .or_else(|| {
            steps
                .iter()
                .position(|step| step.status == PublicActionStepStatus::NotStarted)
        })
        .or_else(|| steps.len().checked_sub(1));
    if let Some(error_index) = error_index {
        for (index, step) in steps.iter_mut().enumerate() {
            if index < error_index && step.status != PublicActionStepStatus::Error {
                step.status = PublicActionStepStatus::Done;
                step.message = None;
            } else if index == error_index {
                step.status = PublicActionStepStatus::Error;
                step.message = Some(Arc::clone(&message));
            } else if step.status != PublicActionStepStatus::Error {
                step.status = PublicActionStepStatus::NotStarted;
                step.message = None;
            }
        }
    }
}

fn private_progress_stage_index(
    steps: &[PrivateBroadcasterProgressStepState],
    stage: TransactionGenerationStage,
) -> usize {
    steps
        .iter()
        .position(|step| step.stage == stage)
        .unwrap_or_else(|| steps.len().saturating_sub(1))
}

pub(in crate::root) const fn self_broadcast_step_retry_kind(
    progress: &PrivateBroadcasterProgressState,
    step: &PrivateBroadcasterProgressStepState,
) -> Option<SelfBroadcastGasRetryKind> {
    match (step.stage, step.status) {
        (TransactionGenerationStage::EstimatingSelfBroadcastGas, PublicActionStepStatus::Error) => {
            Some(SelfBroadcastGasRetryKind::RetryEstimate)
        }
        (TransactionGenerationStage::SigningSelfBroadcast, PublicActionStepStatus::Error) => {
            Some(SelfBroadcastGasRetryKind::RetryStep)
        }
        (
            TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
            PublicActionStepStatus::Pending,
        ) if !progress.self_broadcast_attempts.is_empty() => {
            Some(SelfBroadcastGasRetryKind::SpeedUp)
        }
        _ => None,
    }
}

pub(super) const fn private_broadcaster_retry_button_id(
    kind: SelfBroadcastGasRetryKind,
) -> &'static str {
    match kind {
        SelfBroadcastGasRetryKind::RetryStep => "retry-self-step",
        SelfBroadcastGasRetryKind::RetryEstimate => "retry-self-gas",
        SelfBroadcastGasRetryKind::SpeedUp => "speed-up-self-tx",
    }
}

pub(super) fn private_broadcaster_step_detail(
    progress: &PrivateBroadcasterProgressState,
    step: &PrivateBroadcasterProgressStepState,
    now: Instant,
) -> String {
    if step.status == PublicActionStepStatus::Pending
        && progress.flow == PrivateSubmissionProgressFlow::PublicBroadcaster
        && step.stage == TransactionGenerationStage::WaitingForBroadcasterResponse
        && let Some(detail) = public_broadcaster_wait_status_detail(progress, now)
    {
        return detail;
    }
    private_broadcaster_stage_detail(step.stage, step.status, progress.requires_device_approval)
        .to_string()
}

pub(in crate::root) fn public_broadcaster_wait_status_detail(
    progress: &PrivateBroadcasterProgressState,
    now: Instant,
) -> Option<String> {
    let resend_count = public_broadcaster_resend_count(progress, now)?;
    if resend_count == 0 {
        return Some("Waiting for broadcaster response".to_string());
    }
    let Some(remaining) = public_broadcaster_wait_time_left(progress, now) else {
        return Some(format!("Still waiting - re-sent {resend_count}x"));
    };
    Some(format!(
        "Still waiting - re-sent {resend_count}x - {} left",
        format_public_broadcaster_wait_remaining(remaining)
    ))
}

fn public_broadcaster_resend_count(
    progress: &PrivateBroadcasterProgressState,
    now: Instant,
) -> Option<usize> {
    if progress.flow != PrivateSubmissionProgressFlow::PublicBroadcaster {
        return None;
    }
    let started_at = progress.public_broadcaster_wait_started_at?;
    let republish_interval = progress.public_broadcaster_republish_interval?;
    if republish_interval.is_zero() {
        return None;
    }
    let elapsed = now.saturating_duration_since(started_at);
    let count = elapsed.as_nanos() / republish_interval.as_nanos();
    Some(count.min(usize::MAX as u128) as usize)
}

fn public_broadcaster_wait_time_left(
    progress: &PrivateBroadcasterProgressState,
    now: Instant,
) -> Option<Duration> {
    let started_at = progress.public_broadcaster_wait_started_at?;
    let timeout = progress.public_broadcaster_response_timeout?;
    let elapsed = now.saturating_duration_since(started_at);
    Some(timeout.saturating_sub(elapsed))
}

pub(in crate::root) fn format_public_broadcaster_wait_remaining(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let seconds = total_secs % 60;
    let total_minutes = total_secs / 60;
    if total_minutes < 60 {
        format!("{total_minutes}:{seconds:02}")
    } else {
        let minutes = total_minutes % 60;
        let hours = total_minutes / 60;
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

pub(super) const fn private_broadcaster_stage_label(
    stage: TransactionGenerationStage,
    requires_device_approval: bool,
) -> &'static str {
    if requires_device_approval && matches!(stage, TransactionGenerationStage::SigningSelfBroadcast)
    {
        "Approve self-broadcast transaction"
    } else {
        stage.label()
    }
}

pub(in crate::root) const fn private_broadcaster_stage_detail(
    stage: TransactionGenerationStage,
    status: PublicActionStepStatus,
    requires_device_approval: bool,
) -> &'static str {
    match status {
        PublicActionStepStatus::NotStarted => match stage {
            TransactionGenerationStage::SelectingPrivateNotes => "Waiting to select private notes.",
            TransactionGenerationStage::ProvingTransaction => "Waiting to generate the proof.",
            TransactionGenerationStage::EstimatingBroadcasterFee => {
                "Waiting to settle the transaction fee."
            }
            TransactionGenerationStage::GeneratingPoiProofs => "Waiting to generate POI proofs.",
            TransactionGenerationStage::PublishingToBroadcaster => {
                "Waiting to publish the encrypted request."
            }
            TransactionGenerationStage::WaitingForBroadcasterResponse => {
                "Waiting to listen for broadcaster response."
            }
            TransactionGenerationStage::EstimatingSelfBroadcastGas => "Waiting to estimate gas.",
            TransactionGenerationStage::SigningSelfBroadcast if requires_device_approval => {
                "Waiting for approval on your hardware device."
            }
            TransactionGenerationStage::SigningSelfBroadcast => "Waiting to sign transaction.",
            TransactionGenerationStage::WaitingForSelfBroadcastReceipt => {
                "Waiting for self-broadcast receipt."
            }
        },
        PublicActionStepStatus::Pending
            if requires_device_approval
                && matches!(stage, TransactionGenerationStage::SigningSelfBroadcast) =>
        {
            "Review and approve the transaction on your hardware device."
        }
        PublicActionStepStatus::Pending => stage.detail(),
        PublicActionStepStatus::Done => "Complete.",
        PublicActionStepStatus::Warning => SELF_BROADCAST_INCLUSION_UNOBSERVED_MESSAGE,
        PublicActionStepStatus::Error => "Failed.",
        PublicActionStepStatus::Stopped => {
            "Stopped locally. Already-submitted network work may continue."
        }
    }
}

pub(in crate::root) const fn private_broadcaster_stage_id(
    stage: TransactionGenerationStage,
) -> &'static str {
    match stage {
        TransactionGenerationStage::SelectingPrivateNotes => "select-notes",
        TransactionGenerationStage::ProvingTransaction => "prove",
        TransactionGenerationStage::EstimatingBroadcasterFee => "estimate-fee",
        TransactionGenerationStage::GeneratingPoiProofs => "poi-proofs",
        TransactionGenerationStage::PublishingToBroadcaster => "publish",
        TransactionGenerationStage::WaitingForBroadcasterResponse => "wait-response",
        TransactionGenerationStage::EstimatingSelfBroadcastGas => "estimate-self-gas",
        TransactionGenerationStage::SigningSelfBroadcast => "sign-self-broadcast",
        TransactionGenerationStage::WaitingForSelfBroadcastReceipt => "wait-self-receipt",
    }
}
