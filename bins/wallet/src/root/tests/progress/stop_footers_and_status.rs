use super::*;

#[test]
fn governance_handoff_uses_the_initialized_step_topology() {
    let stake_workflow = [
        PublicActionStepState {
            step: PublicActionProgressStep::GovernanceApprove,
            status: PublicActionStepStatus::Pending,
            tx_hash: None,
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Stake,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
    ];
    assert!(!public_action_step_is_final_handoff_for_steps(
        PublicActionMode::Send,
        PublicActionProgressStep::GovernanceApprove,
        &stake_workflow,
    ));
    assert!(public_action_step_is_final_handoff_for_steps(
        PublicActionMode::Send,
        PublicActionProgressStep::Stake,
        &stake_workflow,
    ));

    let undelegate_workflow = [
        PublicActionStepState {
            step: PublicActionProgressStep::Undelegate,
            status: PublicActionStepStatus::Pending,
            tx_hash: None,
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Unlock,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
    ];
    assert!(!public_action_step_is_final_handoff_for_steps(
        PublicActionMode::Send,
        PublicActionProgressStep::Undelegate,
        &undelegate_workflow,
    ));
    assert!(public_action_step_is_final_handoff_for_steps(
        PublicActionMode::Send,
        PublicActionProgressStep::Unlock,
        &undelegate_workflow,
    ));

    let standalone_undelegate = [PublicActionStepState {
        step: PublicActionProgressStep::Undelegate,
        status: PublicActionStepStatus::Pending,
        tx_hash: None,
        message: None,
        interval: None,
    }];
    assert!(public_action_step_is_final_handoff_for_steps(
        PublicActionMode::Send,
        PublicActionProgressStep::Undelegate,
        &standalone_undelegate,
    ));
}

#[test]
fn private_self_broadcast_stop_footer_follows_signing_handoff_boundary() {
    let key = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let mut progress = private_progress_state(PrivateSubmissionProgressFlow::SelfBroadcast, key);

    assert!(!private_progress_stage_disables_stop(
        PrivateSubmissionProgressFlow::SelfBroadcast,
        false,
        TransactionGenerationStage::EstimatingSelfBroadcastGas,
    ));
    assert!(!sponsored_stop_uses_session_command(
        TransactionGenerationStage::GeneratingPoiProofs,
    ));
    assert!(sponsored_stop_uses_session_command(
        TransactionGenerationStage::SigningSelfBroadcast,
    ));
    assert!(sponsored_stop_uses_session_command(
        TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
    ));
    assert!(private_progress_stage_disables_stop(
        PrivateSubmissionProgressFlow::SelfBroadcast,
        false,
        TransactionGenerationStage::SigningSelfBroadcast,
    ));
    assert!(!private_progress_stage_disables_stop(
        PrivateSubmissionProgressFlow::SelfBroadcast,
        true,
        TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
    ));
    assert_eq!(
        private_broadcaster_progress_footer_action(&progress),
        ProgressFooterAction::Stop,
    );
    progress.stop_available = false;
    assert_eq!(
        private_broadcaster_progress_footer_action(&progress),
        ProgressFooterAction::Close,
    );
}

#[test]
fn private_public_broadcaster_stop_footer_follows_publication_boundary() {
    let key = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let mut progress =
        private_progress_state(PrivateSubmissionProgressFlow::PublicBroadcaster, key);

    assert!(!private_progress_stage_disables_stop(
        PrivateSubmissionProgressFlow::PublicBroadcaster,
        false,
        TransactionGenerationStage::GeneratingPoiProofs,
    ));
    assert!(private_progress_stage_disables_stop(
        PrivateSubmissionProgressFlow::PublicBroadcaster,
        false,
        TransactionGenerationStage::PublishingToBroadcaster,
    ));
    assert_eq!(
        private_broadcaster_progress_footer_action(&progress),
        ProgressFooterAction::Stop,
    );
    progress.stop_available = false;
    assert_eq!(
        private_broadcaster_progress_footer_action(&progress),
        ProgressFooterAction::Close,
    );
}

/// A public broadcaster progress whose route reports `publishes` publishes, and the sender
/// that reports more.
fn progress_with_publishes(
    key: UnshieldAssetKey,
    publishes: u32,
) -> (
    PrivateBroadcasterProgressState,
    tokio::sync::watch::Sender<wallet_ops::SubmissionRouteProgress>,
) {
    let mut progress =
        private_progress_state(PrivateSubmissionProgressFlow::PublicBroadcaster, key);
    let (route, receiver) = tokio::sync::watch::channel(wallet_ops::SubmissionRouteProgress {
        publishes,
        ..wallet_ops::SubmissionRouteProgress::default()
    });
    progress.route = Some(receiver);
    (progress, route)
}

/// Stop waiting follows the re-sends the route made, not the time since the first publish: a
/// route that reconnected through more than two republish intervals re-sent nothing then.
#[test]
fn private_public_broadcaster_stop_footer_reopens_after_two_re_sends() {
    let key = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let (mut progress, route) = progress_with_publishes(key, 2);
    progress.stop_available = false;
    progress.public_broadcaster_wait_started_at = Some(
        std::time::Instant::now()
            .checked_sub(Duration::from_secs(20))
            .expect("started before now"),
    );

    assert_eq!(
        private_broadcaster_progress_footer_action(&progress),
        ProgressFooterAction::Close,
    );

    route.send_modify(|route| route.publishes = 3);

    assert_eq!(
        private_broadcaster_progress_footer_action(&progress),
        ProgressFooterAction::Stop,
    );
}

#[test]
fn public_broadcaster_wait_status_detail_is_concise() {
    let key = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let (mut progress, route) = progress_with_publishes(key, 1);
    let now = std::time::Instant::now();

    progress.public_broadcaster_wait_started_at = Some(now);
    assert_eq!(
        public_broadcaster_wait_status_detail(&progress, now),
        Some("Waiting for broadcaster response".to_string()),
    );

    // 12 s in, two republish intervals passed, and a reconnect let one re-send through. The
    // time left still counts from the wait's start.
    progress.public_broadcaster_wait_started_at = Some(
        now.checked_sub(Duration::from_secs(12))
            .expect("started before now"),
    );
    route.send_modify(|route| route.publishes = 2);
    assert_eq!(
        public_broadcaster_wait_status_detail(&progress, now),
        Some("Still waiting - re-sent 1x - 1:48 left".to_string()),
    );

    progress.public_broadcaster_response_timeout = None;
    assert_eq!(
        public_broadcaster_wait_status_detail(&progress, now),
        Some("Still waiting - re-sent 1x".to_string()),
    );
    assert_eq!(
        format_public_broadcaster_wait_remaining(Duration::from_secs(3661)),
        "1:01:01",
    );
}

/// A job that ends while it proves, by a failure or Stop, closed its route although its
/// delivery never started to report one.
#[test]
fn a_job_ended_before_delivery_shows_its_route_closed() {
    let key = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let proving = || {
        let mut progress =
            private_progress_state(PrivateSubmissionProgressFlow::PublicBroadcaster, key);
        apply_private_broadcaster_progress_stage(
            &mut progress.steps,
            TransactionGenerationStage::ProvingTransaction,
        );
        progress
    };
    let label = |progress: &PrivateBroadcasterProgressState| {
        private_broadcaster_route_card(progress)
            .expect("a public broadcaster submission has a route")
            .indicator()
            .label
    };
    assert_eq!(label(&proving()), "Isolated route");

    let mut failed = proving();
    fail_private_broadcaster_progress_steps_at_stage(
        &mut failed.steps,
        TransactionGenerationStage::ProvingTransaction,
        "The proof could not be generated.",
    );
    assert_eq!(label(&failed), "Route closed");

    let mut stopped = proving();
    stopped.stopped = true;
    mark_private_broadcaster_active_step_stopped(&mut stopped.steps);
    assert_eq!(label(&stopped), "Route closed");
}

#[test]
fn stale_progress_update_guards_reject_stopped_generations() {
    assert!(public_action_accepts_update(7, 7, false));
    assert!(!public_action_accepts_update(7, 6, false));
    assert!(!public_action_accepts_update(7, 7, true));

    let key = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let mut progress =
        private_progress_state(PrivateSubmissionProgressFlow::PublicBroadcaster, key);
    progress.stage_seen = true;
    progress.stopped = true;

    assert_eq!(
        private_broadcaster_closed_active_stage(Some(&progress), DeliveryFormKind::Send, key, 7,),
        None,
    );
}
