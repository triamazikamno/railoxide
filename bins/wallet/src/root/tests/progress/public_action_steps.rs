use super::*;

#[test]
fn semantic_public_action_step_ids_are_unique_and_indexed() {
    let steps = [
        PublicActionProgressStep::ShieldKey,
        PublicActionProgressStep::Send,
        PublicActionProgressStep::Wrap,
        PublicActionProgressStep::Approve,
        PublicActionProgressStep::Shield,
        PublicActionProgressStep::Sponsor,
        PublicActionProgressStep::Unsponsor,
        PublicActionProgressStep::CallVote,
        PublicActionProgressStep::Vote,
        PublicActionProgressStep::GovernanceApprove,
        PublicActionProgressStep::Stake,
        PublicActionProgressStep::Delegate,
        PublicActionProgressStep::Undelegate,
        PublicActionProgressStep::Unlock,
        PublicActionProgressStep::PrincipalClaim,
        PublicActionProgressStep::RewardClaim(0),
        PublicActionProgressStep::RewardClaim(1),
    ];
    let ids = steps
        .into_iter()
        .map(public_action_step_id)
        .collect::<Vec<_>>();

    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        ids.len()
    );
    assert!(ids[15].ends_with("-0"));
    assert!(ids[16].ends_with("-1"));
}

#[test]
fn public_action_failed_step_retry_kind_distinguishes_gas_from_signing_cancel() {
    let gas_error = PublicActionStepState {
        step: PublicActionProgressStep::Wrap,
        status: PublicActionStepStatus::Error,
        tx_hash: None,
        message: Some(Arc::from("public-shield-wrap: estimate gas failed")),
        interval: None,
    };
    let signing_cancel = PublicActionStepState {
        step: PublicActionProgressStep::Wrap,
        status: PublicActionStepStatus::Error,
        tx_hash: None,
        message: Some(Arc::from(
            "public-shield-wrap: hardware sign: Failure_ActionCancelled",
        )),
        interval: None,
    };
    let balance_error = PublicActionStepState {
        message: Some(Arc::from("insufficient native balance for public action")),
        ..gas_error.clone()
    };
    let shield_estimate_error = PublicActionStepState {
        step: PublicActionProgressStep::Shield,
        message: Some(Arc::from(
            "all public action query RPC attempts failed: estimate public action gas: server returned an error response: error code 3: execution reverted, data: \"0x\"",
        )),
        ..gas_error.clone()
    };
    let receipt_error = PublicActionStepState {
        step: PublicActionProgressStep::Shield,
        tx_hash: Some(Arc::from("0x1234")),
        message: Some(Arc::from("shield transaction reverted on-chain")),
        ..gas_error
    };
    let advanced_estimate_error = PublicActionStepState {
        message: Some(Arc::from(
            "re-estimate authorized advanced public transaction gas: execution reverted",
        )),
        ..gas_error.clone()
    };

    assert_eq!(
        public_action_error_retry_kind(&gas_error),
        PublicActionGasRetryKind::RetryEstimate,
    );
    assert_eq!(
        public_action_error_retry_kind(&signing_cancel),
        PublicActionGasRetryKind::RetryStep,
    );
    assert_eq!(
        public_action_error_retry_kind(&balance_error),
        PublicActionGasRetryKind::RetryEstimate,
    );
    for step in [&shield_estimate_error, &advanced_estimate_error] {
        assert_eq!(
            public_action_error_retry_kind(step),
            PublicActionGasRetryKind::RetryEstimate,
        );
        let summary = public_action_error_summary(step.step, step.message.as_deref(), "WETH");
        assert!(summary.starts_with("Could not estimate gas"));
        assert!(!summary.contains("on-chain"));
    }
    assert_eq!(
        public_action_error_retry_kind(&receipt_error),
        PublicActionGasRetryKind::RetryStep,
    );
    assert!(
        public_action_error_summary(receipt_error.step, receipt_error.message.as_deref(), "WETH")
            .contains("reverted on-chain")
    );
    let summary = public_action_error_summary(
        PublicActionProgressStep::Shield,
        balance_error.message.as_deref(),
        "WETH",
    );
    assert!(summary.contains("native balance"));
    assert!(summary.contains("network fees"));
    assert!(!summary.contains("WETH"));
    assert_eq!(
        public_action_error_summary(
            PublicActionProgressStep::Wrap,
            signing_cancel.message.as_deref(),
            "ETH",
        ),
        "Wrapping ETH was cancelled on the hardware wallet.",
    );
}

#[test]
fn public_action_discard_attempt_only_shows_for_active_failed_steps() {
    let failed = PublicActionStepState {
        step: PublicActionProgressStep::Wrap,
        status: PublicActionStepStatus::Error,
        tx_hash: None,
        message: Some(Arc::from("cancelled")),
        interval: None,
    };
    let pending = PublicActionStepState {
        status: PublicActionStepStatus::Pending,
        ..failed.clone()
    };

    assert!(public_action_discard_attempt_available(true, &failed));
    assert!(!public_action_discard_attempt_available(false, &failed));
    assert!(!public_action_discard_attempt_available(true, &pending));
}

#[test]
fn progress_dialog_close_behavior_distinguishes_success_from_failure_and_stop() {
    let success_steps = vec![PublicActionStepState {
        step: PublicActionProgressStep::Send,
        status: PublicActionStepStatus::Done,
        tx_hash: Some(Arc::from("0xabc")),
        message: None,
        interval: None,
    }];
    let failed_steps = vec![PublicActionStepState {
        status: PublicActionStepStatus::Error,
        message: Some(Arc::from("failed")),
        ..success_steps[0].clone()
    }];

    assert!(public_action_progress_is_successful(&success_steps));
    assert!(!public_action_progress_is_successful(&failed_steps));
    assert_eq!(
        progress_dialog_close_behavior(true, false),
        ProgressDialogCloseBehavior::AllAndClear,
    );
    assert_eq!(
        progress_dialog_close_behavior(false, true),
        ProgressDialogCloseBehavior::TopAndClear,
    );
    assert_eq!(
        progress_dialog_close_behavior(false, false),
        ProgressDialogCloseBehavior::TopOnly,
    );
}

#[test]
fn closing_failed_gateway_progress_releases_the_hidden_form_but_hiding_active_work_does_not() {
    let key = UnshieldAssetKey::new(1, Address::from([0x12; 20]));
    let mut progress =
        private_progress_state(PrivateSubmissionProgressFlow::PublicBroadcaster, key);
    progress.kind = DeliveryFormKind::Unshield;
    progress.gateway_execution = Some(wallet_ops::gateway::GatewayDraftExecution::private(
        "extension-operation".into(),
    ));
    assert_eq!(
        private_broadcaster_progress_dialog_close_behavior(&progress),
        ProgressDialogCloseBehavior::TopOnly,
    );
    finish_private_broadcaster_progress_steps_at_stage(
        &mut progress.steps,
        TransactionGenerationStage::WaitingForBroadcasterResponse,
        &PublicBroadcasterResultKind::Failed {
            error: "Rejected by broadcaster".into(),
        },
    );
    assert_eq!(
        private_broadcaster_progress_dialog_close_behavior(&progress),
        ProgressDialogCloseBehavior::AllAndClear,
    );
    progress.gateway_execution = None;
    assert_eq!(
        private_broadcaster_progress_dialog_close_behavior(&progress),
        ProgressDialogCloseBehavior::TopOnly,
    );
    let mut progress = private_progress_state(PrivateSubmissionProgressFlow::SelfBroadcast, key);
    progress.gateway_execution = Some(wallet_ops::gateway::GatewayDraftExecution::private(
        "self-retry".into(),
    ));
    progress.self_broadcast_command_tx = Some(tokio::sync::mpsc::unbounded_channel().0);
    fail_private_broadcaster_progress_steps_at_stage(
        &mut progress.steps,
        TransactionGenerationStage::EstimatingSelfBroadcastGas,
        "retryable gas failure",
    );
    assert_eq!(
        private_broadcaster_progress_dialog_close_behavior(&progress),
        ProgressDialogCloseBehavior::TopOnly
    );
    progress.self_broadcast_result = Some(test_self_broadcast_result(false));
    assert_eq!(
        private_broadcaster_progress_dialog_close_behavior(&progress),
        ProgressDialogCloseBehavior::AllAndClear
    );
}

#[test]
fn replaced_reward_progress_remains_reopenable_but_clear_resets_lifecycle() {
    let history = vec![PublicActionStepState {
        step: PublicActionProgressStep::RewardClaim(0),
        status: PublicActionStepStatus::Done,
        tx_hash: Some(Arc::from("0xconfirmed")),
        message: None,
        interval: None,
    }];
    assert_eq!(
        public_action_progress_handoff_lifecycle(true),
        Some(PublicActionProgressLifecycle::DialogReplacedWhileConfirmedHistoryRemains),
    );
    assert_eq!(public_action_progress_handoff_lifecycle(false), None,);
    assert_eq!(
        public_action_closed_status_step(
            &history,
            PublicActionProgressLifecycle::DialogReplacedWhileConfirmedHistoryRemains,
        )
        .map(|step| step.step),
        Some(PublicActionProgressStep::RewardClaim(0)),
    );
    assert!(
        public_action_closed_status_step(&history, PublicActionProgressLifecycle::Active,)
            .is_none()
    );
}

#[test]
fn stopped_active_step_selection_prefers_pending_error_then_latest_not_started() {
    let mut steps = vec![
        PublicActionStepState {
            step: PublicActionProgressStep::Wrap,
            status: PublicActionStepStatus::Error,
            tx_hash: None,
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Approve,
            status: PublicActionStepStatus::Pending,
            tx_hash: None,
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Shield,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
    ];
    assert!(mark_public_action_active_step_stopped(&mut steps));
    assert_eq!(steps[1].status, PublicActionStepStatus::Stopped);

    let mut steps = vec![
        PublicActionStepState {
            step: PublicActionProgressStep::Approve,
            status: PublicActionStepStatus::Error,
            tx_hash: None,
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Shield,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
    ];
    assert!(mark_public_action_active_step_stopped(&mut steps));
    assert_eq!(steps[0].status, PublicActionStepStatus::Stopped);

    let mut steps = vec![
        PublicActionStepState {
            step: PublicActionProgressStep::Approve,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Shield,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
    ];
    assert!(mark_public_action_active_step_stopped(&mut steps));
    assert_eq!(steps[1].status, PublicActionStepStatus::Stopped);

    let mut private_steps = self_broadcast_progress_steps(DeliveryFormKind::Send);
    assert!(mark_private_broadcaster_active_step_stopped(
        &mut private_steps
    ));
    assert_eq!(private_steps[0].status, PublicActionStepStatus::Stopped);
}

#[test]
fn public_send_stop_footer_follows_send_handoff_boundary() {
    let mut steps = vec![PublicActionStepState {
        step: PublicActionProgressStep::Send,
        status: PublicActionStepStatus::Pending,
        tx_hash: None,
        message: None,
        interval: None,
    }];

    assert_eq!(
        progress_footer_action(true, false),
        ProgressFooterAction::Stop
    );
    assert_eq!(
        public_action_progress_footer_action(true, true, &steps),
        ProgressFooterAction::Stop,
    );
    assert!(public_action_step_is_final_handoff(
        PublicActionMode::Send,
        PublicActionProgressStep::Send,
    ));
    assert_eq!(
        public_action_progress_footer_action(false, true, &steps),
        ProgressFooterAction::Close,
    );

    // A pre-handoff failure waits for a retry and must still accept Cancel/Stop.
    steps[0].status = PublicActionStepStatus::Error;
    assert_eq!(
        public_action_progress_footer_action(true, true, &steps),
        ProgressFooterAction::Stop,
    );
    assert_eq!(
        public_action_progress_footer_action(false, true, &steps),
        ProgressFooterAction::Close,
    );
    assert_eq!(
        public_action_progress_footer_action(true, false, &steps),
        ProgressFooterAction::Close,
    );
}

#[test]
fn erc20_public_shield_stop_footer_allows_approval_until_final_shield() {
    let prerequisite_attempt_steps = vec![
        PublicActionStepState {
            step: PublicActionProgressStep::Approve,
            status: PublicActionStepStatus::Pending,
            tx_hash: Some(Arc::from("0xapprove")),
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Shield,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
    ];

    assert!(!public_action_step_is_final_handoff(
        PublicActionMode::Shield,
        PublicActionProgressStep::Approve,
    ));
    assert!(public_action_step_is_final_handoff(
        PublicActionMode::Shield,
        PublicActionProgressStep::Shield,
    ));
    assert_eq!(
        public_action_progress_footer_action(true, true, &prerequisite_attempt_steps),
        ProgressFooterAction::Stop,
    );
    assert_eq!(
        public_action_progress_footer_action(false, true, &prerequisite_attempt_steps),
        ProgressFooterAction::Close,
    );
}
