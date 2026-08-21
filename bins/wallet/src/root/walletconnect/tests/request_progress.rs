use super::fixtures::*;
use super::*;

#[test]
fn walletconnect_review_token_detects_replaced_request() {
    let mut reviewed = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    reviewed.review_token = 10;
    let mut replacement = test_walletconnect_request("session-topic:7", Some(1_700_000_600));
    replacement.review_token = 11;

    assert!(walletconnect_request_matches_review_token(&reviewed, 10));
    assert!(!walletconnect_request_matches_review_token(
        &replacement,
        10
    ));
}

#[test]
fn walletconnect_transaction_progress_uses_request_specific_steps() {
    let mut request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    request.parsed = WalletConnectParsedRequest::EthSendTransaction {
        transaction: WalletConnectEvmTransaction {
            from: request.item.account,
            to: None,
            value: None,
            data: None,
            access_list: None,
            gas: None,
            gas_price: None,
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            chain_id: None,
            nonce: None,
            transaction_type: None,
            raw: json!({}),
        },
    };

    let progress = WalletConnectApprovalProgress::new(1, &request);

    assert_eq!(progress.generation, 1);
    assert_eq!(
        progress
            .steps
            .iter()
            .map(|step| step.step)
            .collect::<Vec<_>>(),
        vec![
            WalletConnectApprovalProgressStep::PrepareRequest,
            WalletConnectApprovalProgressStep::ApproveOnDevice,
            WalletConnectApprovalProgressStep::BroadcastTransaction,
            WalletConnectApprovalProgressStep::RespondToDapp,
        ]
    );
    assert_eq!(progress.steps[0].status, PublicActionStepStatus::Pending);
    assert!(
        progress.steps[1..]
            .iter()
            .all(|step| step.status == PublicActionStepStatus::NotStarted)
    );
}

#[test]
fn walletconnect_personal_sign_progress_starts_at_device_approval() {
    let mut request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    request.parsed = WalletConnectParsedRequest::PersonalSign {
        message: "0x68656c6c6f".to_owned(),
        account: request.item.account,
    };

    let progress = WalletConnectApprovalProgress::new(2, &request);

    assert_eq!(
        progress
            .steps
            .iter()
            .map(|step| step.step)
            .collect::<Vec<_>>(),
        vec![
            WalletConnectApprovalProgressStep::ApproveOnDevice,
            WalletConnectApprovalProgressStep::RespondToDapp,
        ]
    );
    assert_eq!(progress.steps[0].status, PublicActionStepStatus::Pending);
    assert_eq!(progress.steps[1].status, PublicActionStepStatus::NotStarted);
}

#[test]
fn walletconnect_typed_data_hardware_progress_starts_at_device_approval() {
    let mut request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    request.account_source = PublicAccountSource::HardwareDerived;
    request.item.method = WalletConnectSupportedMethod::EthSignTypedDataV4;
    request.parsed = WalletConnectParsedRequest::EthSignTypedDataV4 {
        account: request.item.account,
        typed_data: json!({}),
        domain_chain_id: Some(U256::from(1_u64)),
    };

    let progress = WalletConnectApprovalProgress::new(4, &request);

    assert_eq!(
        progress
            .steps
            .iter()
            .map(|step| step.step)
            .collect::<Vec<_>>(),
        vec![
            WalletConnectApprovalProgressStep::ApproveOnDevice,
            WalletConnectApprovalProgressStep::RespondToDapp,
        ]
    );
    assert_eq!(progress.steps[0].status, PublicActionStepStatus::Pending);
    assert_eq!(progress.steps[1].status, PublicActionStepStatus::NotStarted);

    request.item.method = WalletConnectSupportedMethod::EthSignTypedData;
    request.parsed = WalletConnectParsedRequest::EthSignTypedData {
        account: request.item.account,
        typed_data: json!({}),
        domain_chain_id: Some(U256::from(1_u64)),
    };
    let unsuffixed_progress = WalletConnectApprovalProgress::new(5, &request);
    assert_eq!(
        unsuffixed_progress
            .steps
            .iter()
            .map(|step| step.step)
            .collect::<Vec<_>>(),
        vec![
            WalletConnectApprovalProgressStep::ApproveOnDevice,
            WalletConnectApprovalProgressStep::RespondToDapp,
        ]
    );
}

#[test]
fn walletconnect_hash_fallback_warning_uses_explicit_continue_label() {
    let mut request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    request.account_source = PublicAccountSource::HardwareDerived;
    request.item.method = WalletConnectSupportedMethod::EthSignTypedDataV4;

    assert!(
        !walletconnect_request_uses_hardware_typed_data_hash_fallback(
            &request,
            HardwareTypedDataSigningMode::ClearSign,
        )
    );
    assert_eq!(
        walletconnect_request_approve_label(false, true, false, false),
        "Approve on device"
    );
    assert!(
        walletconnect_request_uses_hardware_typed_data_hash_fallback(
            &request,
            HardwareTypedDataSigningMode::Eip712HashFallback,
        )
    );
    request.item.method = WalletConnectSupportedMethod::EthSignTypedData;
    assert!(
        walletconnect_request_uses_hardware_typed_data_hash_fallback(
            &request,
            HardwareTypedDataSigningMode::Eip712HashFallback,
        )
    );
    assert_eq!(
        walletconnect_request_approve_label(false, true, true, false),
        "Continue with hash fallback"
    );
    assert_eq!(
        walletconnect_request_approve_label(true, true, true, true),
        "Waiting for device..."
    );
    assert_eq!(
        walletconnect_request_approve_label(false, false, false, true),
        "Approve unlimited"
    );
}

#[test]
fn walletconnect_personal_sign_progress_fails_response_after_device_approval() {
    let mut request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    request.parsed = WalletConnectParsedRequest::PersonalSign {
        message: "0x68656c6c6f".to_owned(),
        account: request.item.account,
    };
    let mut progress = WalletConnectApprovalProgress::new(3, &request);

    progress.apply_update(
        WalletConnectApprovalProgressStep::ApproveOnDevice,
        PublicActionStepStatus::Done,
        None,
        None,
    );
    progress.apply_update(
        WalletConnectApprovalProgressStep::RespondToDapp,
        PublicActionStepStatus::Pending,
        None,
        None,
    );
    progress.fail("relay response timed out".to_owned());

    assert_eq!(progress.steps[0].status, PublicActionStepStatus::Done);
    assert_eq!(progress.steps[1].status, PublicActionStepStatus::Error);
    assert_eq!(
        progress.steps[1].message.as_deref(),
        Some("relay response timed out")
    );
}

#[test]
fn walletconnect_completed_transaction_result_keeps_copyable_tx_hash() {
    let request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    let outcome = WalletConnectRequestApprovalOutcome {
        authorization_failed: false,
        response_published: true,
        submitted_tx_hash: Some("0xabcdef".to_owned()),
        relay_error: None,
        request_error: None,
        expired: false,
        hash_fallback_confirmation_required: false,
        refreshed_hardware_session: None,
    };

    let completed = WalletConnectCompletedRequestUi::from_outcome(request, &outcome);

    assert_eq!(
        completed.status,
        WalletConnectCompletedRequestStatus::TransactionSubmitted
    );
    assert_eq!(completed.submitted_tx_hash.as_deref(), Some("0xabcdef"));
    assert_eq!(
        completed.message.as_ref(),
        "Transaction submitted and WalletConnect response published."
    );
}

#[test]
fn walletconnect_completed_transaction_result_warns_when_response_publish_fails() {
    let request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    let outcome = WalletConnectRequestApprovalOutcome {
        authorization_failed: false,
        response_published: false,
        submitted_tx_hash: Some("0xabcdef".to_owned()),
        relay_error: Some("relay response timed out".to_owned()),
        request_error: None,
        expired: false,
        hash_fallback_confirmation_required: false,
        refreshed_hardware_session: None,
    };

    let completed = WalletConnectCompletedRequestUi::from_outcome(request, &outcome);

    assert_eq!(
        completed.status,
        WalletConnectCompletedRequestStatus::TransactionSubmittedRelayResponseFailed
    );
    assert_eq!(completed.submitted_tx_hash.as_deref(), Some("0xabcdef"));
    assert_eq!(completed.error.as_deref(), Some("relay response timed out"));
}

#[test]
fn walletconnect_completed_request_error_is_not_shown_as_approved() {
    let request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    let outcome = WalletConnectRequestApprovalOutcome {
        authorization_failed: false,
        response_published: true,
        submitted_tx_hash: None,
        relay_error: None,
        request_error: Some("Trezor ActionCancelled".to_owned()),
        expired: false,
        hash_fallback_confirmation_required: false,
        refreshed_hardware_session: None,
    };

    let completed = WalletConnectCompletedRequestUi::from_outcome(request, &outcome);

    assert_eq!(
        completed.status,
        WalletConnectCompletedRequestStatus::RequestFailed
    );
    assert_eq!(completed.error.as_deref(), Some("Trezor ActionCancelled"));
    assert_eq!(
        completed.message.as_ref(),
        "Request was not approved; error response published to the dapp."
    );
}

#[test]
fn walletconnect_hash_fallback_confirmation_outcome_is_local_retry() {
    let outcome = WalletConnectRequestApprovalOutcome::hash_fallback_confirmation_required(None);

    assert!(outcome.hash_fallback_confirmation_required);
    assert!(!outcome.response_published);
    assert!(outcome.submitted_tx_hash.is_none());
    assert!(outcome.relay_error.is_none());
    assert!(outcome.request_error.is_none());
}

#[test]
fn handled_request_keys_suppress_completed_request_replay() {
    let mut pending_requests = BTreeMap::new();
    let mut handled_request_keys = BTreeSet::new();
    let mut handled_request_key_order = VecDeque::new();
    let request = test_walletconnect_request("session-topic:7", None);

    assert!(walletconnect_request_should_queue(
        &pending_requests,
        &handled_request_keys,
        &request.key
    ));

    pending_requests.insert(request.key.clone(), request.clone());
    assert!(!walletconnect_request_should_queue(
        &pending_requests,
        &handled_request_keys,
        &request.key
    ));

    pending_requests.clear();
    remember_walletconnect_handled_request_key(
        &mut handled_request_keys,
        &mut handled_request_key_order,
        request.key.clone(),
        8,
    );

    assert!(!walletconnect_request_should_queue(
        &pending_requests,
        &handled_request_keys,
        &request.key
    ));
}

#[test]
fn auto_open_request_key_skips_dismissed_requests() {
    let mut pending_requests = BTreeMap::new();
    pending_requests.insert(
        "session-topic:1".to_owned(),
        test_walletconnect_request("session-topic:1", None),
    );
    pending_requests.insert(
        "session-topic:2".to_owned(),
        test_walletconnect_request("session-topic:2", None),
    );
    let mut dismissed = BTreeSet::new();

    assert_eq!(
        next_walletconnect_auto_open_request_key(&pending_requests, &dismissed).as_deref(),
        Some("session-topic:1")
    );

    dismissed.insert("session-topic:1".to_owned());
    assert_eq!(
        next_walletconnect_auto_open_request_key(&pending_requests, &dismissed).as_deref(),
        Some("session-topic:2")
    );

    dismissed.insert("session-topic:2".to_owned());
    assert_eq!(
        next_walletconnect_auto_open_request_key(&pending_requests, &dismissed),
        None
    );
}

#[test]
fn manual_request_key_selection_includes_dismissed_requests() {
    let mut pending_requests = BTreeMap::new();
    pending_requests.insert(
        "session-topic:1".to_owned(),
        test_walletconnect_request("session-topic:1", None),
    );

    assert_eq!(
        first_walletconnect_pending_request_key(&pending_requests).as_deref(),
        Some("session-topic:1")
    );
}
