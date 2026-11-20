use super::*;

#[test]
fn closed_private_broadcaster_progress_exposes_active_stage() {
    let key = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let mut progress = PrivateBroadcasterProgressState {
        gateway_execution: None,
        stealth_account: None,
        flow: PrivateSubmissionProgressFlow::PublicBroadcaster,
        kind: DeliveryFormKind::Send,
        key,
        generation_id: 7,
        asset_label: Arc::from("ETH"),
        icon_path: None,
        recipient: Arc::from("0zk"),
        recipient_output: None,
        gas_payer: None,
        requires_device_approval: false,
        sponsored_funding: false,
        steps: private_broadcaster_progress_steps(),
        estimate: None,
        result: None,
        self_broadcast_result: None,
        self_broadcast_command_tx: None,
        sponsored_self_broadcast_command_tx: None,
        sponsored_self_broadcast_outcome: None,
        self_broadcast_attempts: Vec::new(),
        self_broadcast_current_gas_fee: None,
        self_broadcast_action_error: None,
        refusal_retry_pending: false,
        refusal_retry_error: None,
        input_pin_conflict: false,
        public_broadcaster_response_timeout: Some(Duration::from_mins(2)),
        public_broadcaster_wait_started_at: None,
        route: None,
        route_isolated: true,
        task_abort_handle: None,
        payload_inputs_rx: None,
        stop_available: true,
        stopped: false,
        error: None,
        dialog_open: false,
        stage_seen: false,
    };
    assert_eq!(
        private_broadcaster_closed_active_stage(Some(&progress), DeliveryFormKind::Send, key, 7,),
        None
    );
    apply_private_broadcaster_progress_stage(
        &mut progress.steps,
        TransactionGenerationStage::PublishingToBroadcaster,
    );
    progress.stage_seen = true;

    assert_eq!(
        private_broadcaster_closed_active_stage(Some(&progress), DeliveryFormKind::Send, key, 7,),
        Some(TransactionGenerationStage::PublishingToBroadcaster)
    );

    progress.dialog_open = true;
    assert_eq!(
        private_broadcaster_closed_active_stage(Some(&progress), DeliveryFormKind::Send, key, 7,),
        None
    );

    progress.dialog_open = false;
    progress.error = Some(Arc::from("failed"));
    assert_eq!(
        private_broadcaster_closed_active_stage(Some(&progress), DeliveryFormKind::Send, key, 7,),
        None
    );
}

#[test]
fn closed_private_broadcaster_progress_exposes_failed_step_for_discard() {
    let key = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let mut progress = private_progress_state(PrivateSubmissionProgressFlow::SelfBroadcast, key);
    progress.stage_seen = true;
    fail_private_broadcaster_progress_steps_at_stage(
        &mut progress.steps,
        TransactionGenerationStage::SigningSelfBroadcast,
        "hardware sign: Failure_ActionCancelled",
    );

    let active =
        private_broadcaster_closed_active_progress(Some(&progress), DeliveryFormKind::Send, key, 7)
            .expect("active failed step");

    assert_eq!(active.flow, PrivateSubmissionProgressFlow::SelfBroadcast);
    assert_eq!(
        active.step.stage,
        TransactionGenerationStage::SigningSelfBroadcast
    );
    assert_eq!(active.step.status, PublicActionStepStatus::Error);
    assert!(private_submission_discard_attempt_available(&active));
}

#[test]
fn private_self_broadcast_step_retry_kind_separates_signing_from_gas_and_speed_up() {
    let key = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let mut progress = private_progress_state(PrivateSubmissionProgressFlow::SelfBroadcast, key);
    let gas_step = PrivateBroadcasterProgressStepState {
        stage: TransactionGenerationStage::EstimatingSelfBroadcastGas,
        status: PublicActionStepStatus::Error,
        message: Some(Arc::from("gas failed")),
    };
    let signing_step = PrivateBroadcasterProgressStepState {
        stage: TransactionGenerationStage::SigningSelfBroadcast,
        status: PublicActionStepStatus::Error,
        message: Some(Arc::from("cancelled")),
    };
    let receipt_step = PrivateBroadcasterProgressStepState {
        stage: TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
        status: PublicActionStepStatus::Pending,
        message: None,
    };

    assert_eq!(
        self_broadcast_step_retry_kind(&progress, &gas_step),
        Some(SelfBroadcastGasRetryKind::RetryEstimate),
    );
    assert_eq!(
        self_broadcast_step_retry_kind(&progress, &signing_step),
        Some(SelfBroadcastGasRetryKind::RetryStep),
    );
    assert_eq!(
        self_broadcast_step_retry_kind(&progress, &receipt_step),
        None
    );
    progress
        .self_broadcast_attempts
        .push(SelfBroadcastAttemptInfo {
            tx_hash: "0xabc".to_string(),
            nonce: 7,
            gas_limit: 21_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
        });
    assert_eq!(
        self_broadcast_step_retry_kind(&progress, &receipt_step),
        Some(SelfBroadcastGasRetryKind::SpeedUp),
    );
}

#[test]
fn private_self_broadcast_success_requires_successful_receipt() {
    use wallet_ops::gateway::GatewayPrivateDraftResult as ResultKind;
    let key = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let mut progress = private_progress_state(PrivateSubmissionProgressFlow::SelfBroadcast, key);
    assert!(gateway_self_broadcast_result(&progress).is_none());
    progress.self_broadcast_result = Some(test_self_broadcast_result(true));
    assert!(private_broadcaster_progress_is_successful(&progress));
    assert_eq!(
        gateway_self_broadcast_result(&progress),
        Some((ResultKind::Confirmed, Some("0xabc".into())))
    );

    progress.self_broadcast_result = Some(test_self_broadcast_result(false));
    assert_eq!(
        gateway_self_broadcast_result(&progress).unwrap().0,
        ResultKind::Reverted
    );
    assert!(!private_broadcaster_progress_is_successful(&progress));

    // An outer success must not become browser Confirmed before the account's nonce is
    // read past the payload's.
    for (state, expected) in [
        (
            wallet_ops::vault::ExecutorPayloadState::Pending,
            ResultKind::Submitted,
        ),
        (
            wallet_ops::vault::ExecutorPayloadState::Resolved,
            ResultKind::Confirmed,
        ),
    ] {
        let mut result = test_self_broadcast_result(true);
        result.tx = wallet_ops::SelfBroadcastTxOutcome::ExecutorReceipt {
            receipt: result.tx.receipt().unwrap().clone(),
            state,
        };
        progress.self_broadcast_result = Some(result);
        assert_eq!(
            gateway_self_broadcast_result(&progress).unwrap().0,
            expected
        );
        assert_eq!(
            private_broadcaster_progress_is_successful(&progress),
            expected == ResultKind::Confirmed
        );
        assert_eq!(
            crate::root::private_broadcaster::gateway_self_broadcast_status(expected).0,
            wallet_ops::gateway::GatewayDraftStatus::Done,
            "A submitted executor remains a handed-off browser result, not a stopped action",
        );
    }

    let mut result = test_self_broadcast_result(true);
    result.tx = wallet_ops::SelfBroadcastTxOutcome::InclusionUnobserved {
        tx_hash: "0xunobserved".to_string(),
    };
    result.attempts.push(SelfBroadcastAttemptInfo {
        tx_hash: result.tx.tx_hash().to_owned(),
        nonce: 7,
        gas_limit: result.gas_limit,
        max_fee_per_gas: result.max_fee_per_gas,
        max_priority_fee_per_gas: result.max_priority_fee_per_gas,
    });
    finish_private_self_broadcast_progress_steps_at_stage(
        &mut progress.steps,
        TransactionGenerationStage::WaitingForSelfBroadcastReceipt,
        result.tx.execution_status(),
    );
    progress
        .self_broadcast_attempts
        .clone_from(&result.attempts);
    progress.self_broadcast_result = Some(result);
    assert_eq!(
        gateway_self_broadcast_result(&progress),
        Some((ResultKind::InclusionUnknown, Some("0xunobserved".into())))
    );
    assert!(!private_broadcaster_progress_is_successful(&progress));
    assert!(private_broadcaster_progress_is_terminal(&progress));
    assert_eq!(
        private_broadcaster_progress_footer_action(&progress),
        ProgressFooterAction::Close,
    );
    let receipt_step = progress.steps.last().expect("receipt step");
    assert_eq!(receipt_step.status, PublicActionStepStatus::Warning);
    assert!(receipt_step.message.is_some());
    assert!(
        progress.steps[..progress.steps.len() - 1]
            .iter()
            .all(|step| step.status == PublicActionStepStatus::Done)
    );
    assert_eq!(
        self_broadcast_step_retry_kind(&progress, receipt_step),
        None
    );
    assert_eq!(
        progress
            .self_broadcast_result
            .as_ref()
            .expect("result retained")
            .tx
            .tx_hash(),
        "0xunobserved",
    );
    assert!(progress.error.is_none());
    progress.self_broadcast_result = None;
    assert!(
        gateway_self_broadcast_result(&progress).is_none(),
        "submitted attempts are not confirmation"
    );
    for accepted in [false, true] {
        progress.sponsored_self_broadcast_outcome =
            Some(SponsoredSelfBroadcastSessionOutcome::Stopped {
                reason: wallet_ops::SponsoredSelfBroadcastStopReason::Cancelled,
                bundle_was_accepted: accepted,
            });
        assert_eq!(
            gateway_self_broadcast_result(&progress).unwrap().0,
            if accepted {
                ResultKind::InclusionUnknown
            } else {
                ResultKind::Stopped
            }
        );
    }
    for success in [false, true] {
        progress.sponsored_self_broadcast_outcome =
            Some(SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(
                test_self_broadcast_result(success)
                    .tx
                    .receipt()
                    .unwrap()
                    .clone(),
            ));
        assert_eq!(
            gateway_self_broadcast_result(&progress).unwrap().0,
            if success {
                ResultKind::Confirmed
            } else {
                ResultKind::Reverted
            }
        );
    }
}

#[test]
fn self_broadcast_composite_output_rows_show_primary_and_top_up_outputs() {
    let key = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let mut progress = private_progress_state(PrivateSubmissionProgressFlow::SelfBroadcast, key);
    progress.kind = DeliveryFormKind::Unshield;
    progress.recipient_output = Some(Arc::from("100 USDC"));
    let mut result = test_self_broadcast_result(true);
    result.native_top_up = Some(DesktopNativeTopUpPlan {
        recipient: Address::from([0x42; 20]),
        wrapped_native_token: Address::from([0x43; 20]),
        native_amount: uint!(3_000_000_000_000_000_U256),
        wrapped_native_amount: wallet_ops::native_top_up_wrapped_native_amount(uint!(
            3_000_000_000_000_000_U256
        )),
    });

    assert_eq!(
        self_broadcast_composite_output_rows(&progress, &result),
        vec![(
            "Recipient receives",
            "100 USDC + 0.003 ETH (gas top-up)".to_string()
        )]
    );
}

/// Banned wins over every other state, the favorite state shows only after submission, and
/// Ban only while the flow offers it before a submission.
#[test]
fn broadcaster_preference_control_follows_ban_then_submission() {
    use BroadcasterPreferenceControl::{AddFavorite, Ban, Banned, Favorited};
    // (banned, favorite, submitted, ban_offered)
    let cases = [
        ((true, true, true, true), Some(Banned)),
        ((true, false, false, false), Some(Banned)),
        ((false, true, true, true), Some(Favorited)),
        ((false, false, true, true), Some(AddFavorite)),
        ((false, true, false, false), None),
        ((false, true, false, true), Some(Ban)),
        ((false, false, false, true), Some(Ban)),
        ((false, false, false, false), None),
    ];
    for ((banned, favorite, submitted, ban_offered), expected) in cases {
        assert_eq!(
            broadcaster_preference_control(banned, favorite, submitted, ban_offered),
            expected,
            "banned={banned} favorite={favorite} submitted={submitted} ban_offered={ban_offered}"
        );
    }
}

/// Stopping or resetting a running attempt skips its completion, so the payload it already
/// sent must still pin the form that a retry reuses.
#[gpui::test]
fn abandoned_public_broadcaster_attempts_pin_their_payload_notes(cx: &mut gpui::TestAppContext) {
    use wallet_ops::{PublicBroadcasterInputPin, SelectedInputIdentity};
    let path = temp_wallet_db_root("abandoned-attempt-pin");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let (root, cx) = wallet_view_root(&path, &runtime, cx);
    let asset = UnshieldAsset {
        chain_id: 1,
        token: Address::from([0x31; 20]),
        label: "WETH".to_string(),
        decimals: Some(18),
        total: U256::from(10),
        poi_verified_total: U256::from(10),
        max_batched: U256::from(10),
        icon_path: None,
    };
    let key = UnshieldAssetKey::from_asset(&asset);
    let note = |position| SelectedInputIdentity { tree: 0, position };
    // A running attempt of the form's next generation whose payload already went out.
    let start_attempt = |root: &mut WalletRoot, inputs: Vec<SelectedInputIdentity>| {
        let form = root.send_forms.get_mut(&key).expect("send form");
        form.generation_id += 1;
        form.generating = true;
        let mut progress =
            private_progress_state(PrivateSubmissionProgressFlow::PublicBroadcaster, key);
        progress.generation_id = form.generation_id;
        let (payload_inputs_tx, payload_inputs_rx) = tokio::sync::oneshot::channel();
        payload_inputs_tx.send(inputs).expect("payload inputs");
        progress.payload_inputs_rx = Some(payload_inputs_rx);
        root.private_broadcaster_progress = Some(progress);
    };
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.initialize_send_form(asset, window, cx);
            start_attempt(root, vec![note(1), note(2)]);
            root.stop_private_broadcaster_progress(cx);
            assert_eq!(
                root.send_forms[&key].input_pin,
                Some(PublicBroadcasterInputPin {
                    inputs: vec![note(1), note(2)],
                    overridden: false,
                })
            );

            // A cache reset abandons the retry while keeping the form; the pin narrows to the
            // note both proofs spend.
            start_attempt(root, vec![note(2), note(3)]);
            let _ = root.begin_public_sync_cache_reset(cx);
            assert_eq!(
                root.send_forms[&key].input_pin,
                Some(PublicBroadcasterInputPin {
                    inputs: vec![note(2)],
                    overridden: false,
                })
            );
        });
    });

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}
