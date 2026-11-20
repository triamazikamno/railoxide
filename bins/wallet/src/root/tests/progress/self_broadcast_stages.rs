use super::*;

#[test]
fn self_broadcast_progress_stage_sequence_tracks_direct_submission() {
    let mut steps = self_broadcast_progress_steps(DeliveryFormKind::Send);

    assert_eq!(
        steps.iter().map(|step| step.stage).collect::<Vec<_>>(),
        vec![
            TransactionGenerationStage::SelectingPrivateNotes,
            TransactionGenerationStage::ProvingTransaction,
            TransactionGenerationStage::GeneratingPoiProofs,
            TransactionGenerationStage::EstimatingSelfBroadcastGas,
            TransactionGenerationStage::SigningSelfBroadcast,
            TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
        ]
    );

    apply_private_broadcaster_progress_stage(
        &mut steps,
        TransactionGenerationStage::SigningSelfBroadcast,
    );
    let statuses = steps.iter().map(|step| step.status).collect::<Vec<_>>();
    assert_eq!(
        statuses,
        vec![
            PublicActionStepStatus::Done,
            PublicActionStepStatus::Done,
            PublicActionStepStatus::Done,
            PublicActionStepStatus::Done,
            PublicActionStepStatus::Pending,
            PublicActionStepStatus::NotStarted,
        ]
    );
}

#[test]
fn self_broadcast_unshield_progress_omits_poi_until_requested() {
    let mut steps = self_broadcast_progress_steps(DeliveryFormKind::Unshield);

    assert_eq!(
        steps.iter().map(|step| step.stage).collect::<Vec<_>>(),
        vec![
            TransactionGenerationStage::SelectingPrivateNotes,
            TransactionGenerationStage::ProvingTransaction,
            TransactionGenerationStage::EstimatingSelfBroadcastGas,
            TransactionGenerationStage::SigningSelfBroadcast,
            TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
        ]
    );

    apply_private_broadcaster_progress_stage(
        &mut steps,
        TransactionGenerationStage::EstimatingSelfBroadcastGas,
    );
    assert!(
        !steps
            .iter()
            .any(|step| step.stage == TransactionGenerationStage::GeneratingPoiProofs)
    );
    ensure_self_broadcast_unshield_progress_stage(
        &mut steps,
        TransactionGenerationStage::GeneratingPoiProofs,
    );
    assert_eq!(steps[2].status, PublicActionStepStatus::Done);

    let mut steps = self_broadcast_progress_steps(DeliveryFormKind::Unshield);
    ensure_self_broadcast_unshield_progress_stage(
        &mut steps,
        TransactionGenerationStage::GeneratingPoiProofs,
    );
    apply_private_broadcaster_progress_stage(
        &mut steps,
        TransactionGenerationStage::GeneratingPoiProofs,
    );
    assert_eq!(
        steps.iter().map(|step| step.stage).collect::<Vec<_>>(),
        vec![
            TransactionGenerationStage::SelectingPrivateNotes,
            TransactionGenerationStage::ProvingTransaction,
            TransactionGenerationStage::GeneratingPoiProofs,
            TransactionGenerationStage::EstimatingSelfBroadcastGas,
            TransactionGenerationStage::SigningSelfBroadcast,
            TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
        ]
    );
    assert_eq!(steps[2].status, PublicActionStepStatus::Pending);
}

#[test]
fn self_broadcast_reverted_receipt_marks_receipt_step_error() {
    let mut steps = self_broadcast_progress_steps(DeliveryFormKind::Send);

    finish_private_self_broadcast_progress_steps_at_stage(
        &mut steps,
        TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
        Some(false),
    );

    assert_eq!(
        steps.last().map(|step| step.status),
        Some(PublicActionStepStatus::Error)
    );
    assert!(
        steps[..steps.len() - 1]
            .iter()
            .all(|step| step.status == PublicActionStepStatus::Done)
    );
    assert_eq!(
        steps.last().and_then(|step| step.message.as_deref()),
        Some("Transaction receipt indicates the self-broadcast transaction reverted.")
    );
}

#[test]
fn stopped_progress_status_uses_danger_stop_marker_and_copy() {
    assert_eq!(
        public_action_step_color(PublicActionStepStatus::Stopped),
        ui::theme::DANGER,
    );
    assert!(public_action_step_uses_stop_marker(
        PublicActionStepStatus::Stopped
    ));
    assert_eq!(
        public_action_step_detail(
            PublicActionProgressStep::Send,
            PublicActionStepStatus::Stopped,
        ),
        "Stopped locally. Already-submitted network work may continue.",
    );
    assert_eq!(
        public_action_step_detail_for_context(
            PublicActionProgressStep::ShieldKey,
            PublicActionStepStatus::Pending,
            true,
            false,
        ),
        "Approve the RAILGUN_SHIELD message on your hardware wallet.",
    );
    assert_eq!(
        public_action_step_detail_for_context(
            PublicActionProgressStep::Approve,
            PublicActionStepStatus::Pending,
            true,
            false,
        ),
        "Approve the transaction on your hardware wallet, then wait for broadcast.",
    );
}
