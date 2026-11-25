use super::*;

#[tokio::test]
async fn sponsored_cleanup_aborts_preparation_but_drains_active_session() {
    let key = UnshieldAssetKey::new(1, Address::from([0x12; 20]));
    let (prepare_tx, mut prepare_rx) =
        tokio::sync::watch::channel(SponsoredSelfBroadcastCommand::Running);
    let prepare_task = tokio::spawn(std::future::pending::<()>());
    let mut preparing = private_progress_state(PrivateSubmissionProgressFlow::SelfBroadcast, key);
    preparing.sponsored_funding = true;
    preparing.sponsored_self_broadcast_command_tx = Some(prepare_tx);
    preparing.task_abort_handle = Some(prepare_task.abort_handle());
    apply_private_broadcaster_progress_stage(
        &mut preparing.steps,
        TransactionGenerationStage::GeneratingPoiProofs,
    );

    assert!(cancel_private_broadcaster_progress(&mut Some(preparing)).is_none());
    prepare_rx.changed().await.expect("preparation shutdown");
    assert_eq!(
        *prepare_rx.borrow(),
        SponsoredSelfBroadcastCommand::Shutdown
    );
    assert!(
        prepare_task
            .await
            .expect_err("preparation aborted")
            .is_cancelled()
    );

    let (session_tx, mut session_rx) =
        tokio::sync::watch::channel(SponsoredSelfBroadcastCommand::Running);
    let session_task = tokio::spawn(std::future::pending::<()>());
    let mut waiting = private_progress_state(PrivateSubmissionProgressFlow::SelfBroadcast, key);
    waiting.sponsored_funding = true;
    waiting.sponsored_self_broadcast_command_tx = Some(session_tx);
    waiting.task_abort_handle = Some(session_task.abort_handle());
    apply_private_broadcaster_progress_stage(
        &mut waiting.steps,
        TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
    );

    let retained = cancel_private_broadcaster_progress(&mut Some(waiting))
        .expect("active session task retained for cleanup");
    session_rx.changed().await.expect("session shutdown");
    assert_eq!(
        *session_rx.borrow(),
        SponsoredSelfBroadcastCommand::Shutdown
    );
    assert!(!retained.is_finished());
    retained.abort();
    let _ = session_task.await;
}

#[tokio::test]
async fn sponsored_abort_watchdog_bounds_an_unresponsive_session() {
    let task = tokio::spawn(std::future::pending::<()>());
    spawn_sponsored_abort_watchdog(
        &tokio::runtime::Handle::current(),
        task.abort_handle(),
        Duration::from_millis(20),
    );

    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("watchdog completes within bound")
        .expect_err("watchdog aborts task");
    assert!(result.is_cancelled());
}

#[tokio::test]
async fn sponsored_cancel_retains_sender_for_later_shutdown() {
    let key = UnshieldAssetKey::new(1, Address::from([0x13; 20]));
    let (command_tx, mut command_rx) =
        tokio::sync::watch::channel(SponsoredSelfBroadcastCommand::Running);
    let mut progress = private_progress_state(PrivateSubmissionProgressFlow::SelfBroadcast, key);
    progress.sponsored_self_broadcast_command_tx = Some(command_tx);

    assert!(request_sponsored_session_cancel(&mut progress));
    command_rx.changed().await.expect("cancel command");
    assert_eq!(
        *command_rx.borrow_and_update(),
        SponsoredSelfBroadcastCommand::Cancel
    );

    progress
        .sponsored_self_broadcast_command_tx
        .as_ref()
        .expect("sender retained")
        .send(SponsoredSelfBroadcastCommand::Shutdown)
        .expect("shutdown command");
    command_rx
        .changed()
        .await
        .expect("shutdown command observed");
    assert_eq!(
        *command_rx.borrow(),
        SponsoredSelfBroadcastCommand::Shutdown
    );
}

#[test]
fn private_broadcaster_progress_stage_marks_prior_steps_done() {
    let mut steps = private_broadcaster_progress_steps();

    apply_private_broadcaster_progress_stage(
        &mut steps,
        TransactionGenerationStage::EstimatingBroadcasterFee,
    );

    let statuses = steps.iter().map(|step| step.status).collect::<Vec<_>>();
    assert_eq!(
        statuses,
        vec![
            PublicActionStepStatus::Done,
            PublicActionStepStatus::Done,
            PublicActionStepStatus::Pending,
            PublicActionStepStatus::NotStarted,
            PublicActionStepStatus::NotStarted,
            PublicActionStepStatus::NotStarted,
        ]
    );
}

#[test]
fn private_broadcaster_progress_submitted_marks_all_steps_done() {
    let mut steps = private_broadcaster_progress_steps();
    apply_private_broadcaster_progress_stage(
        &mut steps,
        TransactionGenerationStage::WaitingForBroadcasterResponse,
    );

    finish_private_broadcaster_progress_steps(
        &mut steps,
        &PublicBroadcasterResultKind::Submitted {
            tx_hash: alloy::primitives::B256::repeat_byte(0xab),
        },
    );

    assert!(
        steps
            .iter()
            .all(|step| step.status == PublicActionStepStatus::Done)
    );
}

#[test]
fn private_broadcaster_progress_timeout_marks_waiting_step_error() {
    let mut steps = private_broadcaster_progress_steps();
    apply_private_broadcaster_progress_stage(
        &mut steps,
        TransactionGenerationStage::WaitingForBroadcasterResponse,
    );

    finish_private_broadcaster_progress_steps(&mut steps, &PublicBroadcasterResultKind::TimedOut);

    assert_eq!(
        steps.last().map(|step| step.status),
        Some(PublicActionStepStatus::Error)
    );
    assert!(
        steps
            .last()
            .and_then(|step| step.message.as_ref())
            .is_some()
    );
}

#[test]
fn private_broadcaster_terminal_failure_applies_latest_stage() {
    let mut steps = private_broadcaster_progress_steps();

    fail_private_broadcaster_progress_steps_at_stage(
        &mut steps,
        TransactionGenerationStage::PublishingToBroadcaster,
        "publish failed",
    );

    let statuses = steps.iter().map(|step| step.status).collect::<Vec<_>>();
    assert_eq!(
        statuses,
        vec![
            PublicActionStepStatus::Done,
            PublicActionStepStatus::Done,
            PublicActionStepStatus::Done,
            PublicActionStepStatus::Done,
            PublicActionStepStatus::Error,
            PublicActionStepStatus::NotStarted,
        ]
    );
    assert_eq!(steps[4].message.as_deref(), Some("publish failed"));
}

#[test]
fn private_broadcaster_terminal_result_applies_latest_stage_before_timeout() {
    let mut steps = private_broadcaster_progress_steps();

    finish_private_broadcaster_progress_steps_at_stage(
        &mut steps,
        TransactionGenerationStage::WaitingForBroadcasterResponse,
        &PublicBroadcasterResultKind::TimedOut,
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
}
