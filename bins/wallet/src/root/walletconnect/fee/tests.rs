use super::*;
use alloy::primitives::Address;
use wallet_ops::WalletConnectEvmTransaction;

#[tokio::test]
async fn gateway_fee_quote_discards_a_delayed_read_after_owner_invalidation() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let released = Arc::new(tokio::sync::Notify::new());
    let reads = wallet_ops::DappRpcReadClient::new({
        let entered = entered.clone();
        let released = released.clone();
        move |_, _| {
            let entered = entered.clone();
            let released = released.clone();
            Box::pin(async move {
                entered.notify_one();
                released.notified().await;
                Ok(serde_json::json!("0x3b9aca00"))
            })
        }
    });
    let control = wallet_ops::dapp_request::DappRequestControl::new(
        tokio::time::Instant::now() + std::time::Duration::from_mins(5),
        || Ok(()),
    );
    let http = wallet_ops::build_http_client(None).unwrap();
    let task = tokio::spawn(quote_walletconnect_fee_with_retry(
        1,
        wallet_ops::settings::build_effective_chain_configs(
            &wallet_ops::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap(),
        http,
        None,
        Some(reads),
        Some(control.clone()),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    control.invalidate(&wallet_ops::RpcBrokerError::OriginRejected);
    released.notify_one();
    assert!(
        task.await.unwrap().is_err(),
        "a successful stale read must not become a fee quote"
    );
}

#[tokio::test(start_paused = true)]
async fn gateway_fee_quote_stops_retrying_when_read_owner_retires_before_control() {
    for error in [
        wallet_ops::RpcBrokerError::OriginRejected,
        wallet_ops::RpcBrokerError::Shutdown,
    ] {
        let read_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reads = wallet_ops::DappRpcReadClient::new({
            let read_started = read_started.clone();
            move |_, _| {
                read_started.store(true, std::sync::atomic::Ordering::SeqCst);
                let error = error.clone();
                Box::pin(async move { Err(error) })
            }
        });
        let control = wallet_ops::dapp_request::DappRequestControl::new(
            tokio::time::Instant::now() + std::time::Duration::from_mins(5),
            || Ok(()),
        );
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            quote_walletconnect_fee_with_retry(
                1,
                wallet_ops::settings::build_effective_chain_configs(
                    &wallet_ops::settings::WalletSettings::default(),
                )
                .unwrap()
                .get(1)
                .cloned()
                .unwrap(),
                wallet_ops::build_http_client(None).unwrap(),
                None,
                Some(reads),
                Some(control.clone()),
            ),
        )
        .await
        .expect("a retired read owner must stop fee retries before the first backoff");
        assert!(read_started.load(std::sync::atomic::Ordering::SeqCst));
        assert!(result.is_err());
        assert!(
            control.ensure_current().is_ok(),
            "response delivery still owns the control"
        );
    }
}

fn test_fee_state() -> WalletConnectFeeState {
    WalletConnectFeeState {
        request_key: Arc::from("topic:7"),
        review_token: 4,
        payload_fingerprint: B256::ZERO,
        request_generation: 1,
        dialog_generation: 2,
        editor_generation: 3,
        expiry_timestamp: None,
        status: WalletConnectFeeStatus::Fetching,
        error: None,
        retry_attempt: 0,
        generation: 0,
        simulation_requested: false,
        simulation_retryable: false,
        last_successful_display_projection: None,
    }
}

fn test_projection(
    source: PublicActionFeeSource,
    expected_gas_cost: u64,
) -> PublicActionFeeProjection {
    PublicActionFeeProjection {
        source,
        raw_gas_limit: 21_000,
        gas_limit: 31_000,
        max_fee_per_gas: 20,
        max_priority_fee_per_gas: 2,
        expected_fee_per_gas: 15,
        expected_gas_cost: U256::from(expected_gas_cost),
        maximum_gas_cost: U256::from(expected_gas_cost + 10),
        expected_native_usd_micro_value: None,
        maximum_native_usd_micro_value: None,
    }
}

#[test]
fn transaction_fee_review_pairing_matches_request_kind() {
    let transaction = WalletConnectParsedRequest::EthSendTransaction {
        transaction: WalletConnectEvmTransaction {
            from: Address::ZERO,
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
            raw: serde_json::Value::Null,
        },
    };
    let personal = WalletConnectParsedRequest::PersonalSign {
        message: "hello".to_owned(),
        account: Address::ZERO,
    };
    let typed_data = WalletConnectParsedRequest::EthSignTypedDataV4 {
        account: Address::ZERO,
        typed_data: serde_json::Value::Null,
        domain_chain_id: None,
    };

    assert!(walletconnect_request_fee_eligible(&transaction));
    assert!(!walletconnect_request_fee_eligible(&personal));
    assert!(!walletconnect_request_fee_eligible(&typed_data));

    let reviewed_fee = WalletConnectReviewedFeeProjection::unresolved("topic:7", 1);
    assert!(
        validate_walletconnect_reviewed_fee_pairing(&transaction, Some(&reviewed_fee),).is_ok()
    );
    assert!(validate_walletconnect_reviewed_fee_pairing(&typed_data, None).is_ok());
    assert!(validate_walletconnect_reviewed_fee_pairing(&transaction, None).is_err());
    assert!(
        validate_walletconnect_reviewed_fee_pairing(&typed_data, Some(&reviewed_fee),).is_err()
    );
}

#[test]
fn custom_projection_does_not_require_an_automatic_quote() {
    let (projection, resolved) = walletconnect_fee_projection(
        1,
        21_000,
        10_000,
        None,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 2,
        },
        None,
        18,
    )
    .expect("custom projection");
    assert_eq!(resolved.max_fee_per_gas, 20);
    assert_eq!(projection.raw_gas_limit, 21_000);
    assert_eq!(projection.gas_limit, 31_000);
}

#[test]
fn active_refresh_uses_display_projection_without_authoritative_status() {
    let projection = test_projection(PublicActionFeeSource::OperationTable, 315_000);
    let mut state = test_fee_state();
    state.last_successful_display_projection = Some(projection);

    assert!(matches!(state.status, WalletConnectFeeStatus::Fetching));
    assert_eq!(state.authoritative_projection(), None);
    assert_eq!(
        walletconnect_fee_state_projection(&state, true),
        Some(projection)
    );
}

#[test]
fn inactive_terminal_failure_does_not_use_display_projection() {
    let projection = test_projection(PublicActionFeeSource::NetworkSimulation, 315_000);
    let mut state = test_fee_state();
    state.status = WalletConnectFeeStatus::UnavailableFailed;
    state.last_successful_display_projection = Some(projection);

    assert_eq!(walletconnect_fee_state_projection(&state, false), None);
}

#[test]
fn editor_reset_and_new_request_clear_display_projection() {
    let projection = test_projection(PublicActionFeeSource::OperationTable, 315_000);
    let mut state = test_fee_state();
    state.last_successful_display_projection = Some(projection);
    state.invalidate();
    assert_eq!(state.last_successful_display_projection, None);

    let replacement = test_fee_state();
    assert_eq!(replacement.last_successful_display_projection, None);
}

#[test]
fn successful_projection_replaces_display_projection() {
    let old_projection = test_projection(PublicActionFeeSource::OperationTable, 315_000);
    let new_projection = test_projection(PublicActionFeeSource::NetworkSimulation, 420_000);
    let mut state = test_fee_state();
    state.apply_successful_operation_projection(old_projection);
    assert_eq!(
        state.status,
        WalletConnectFeeStatus::EstimatedFromOperation(old_projection)
    );
    assert_eq!(state.authoritative_projection(), Some(old_projection));
    assert_eq!(
        state.last_successful_display_projection,
        Some(old_projection)
    );

    state.apply_successful_simulation_projection(new_projection);

    assert_eq!(
        walletconnect_fee_state_projection(&state, false),
        Some(new_projection)
    );
    assert_eq!(state.authoritative_projection(), Some(new_projection));
    assert_eq!(
        state.status,
        WalletConnectFeeStatus::Simulated(new_projection)
    );
    assert_eq!(
        state.last_successful_display_projection,
        Some(new_projection)
    );
}

#[test]
fn reviewed_fee_context_rejects_stale_request_and_editor_selection() {
    let mut reviewed = WalletConnectReviewedFeeProjection::unresolved("topic:7", 4);
    reviewed.payload_fingerprint = B256::from([7; 32]);
    reviewed.editor_generation = 3;
    reviewed.selection = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 20,
        max_priority_fee_per_gas: 2,
    };
    assert!(walletconnect_reviewed_fee_request_context_matches(
        &reviewed,
        "topic:7",
        4,
        B256::from([7; 32]),
    ));
    assert!(!walletconnect_reviewed_fee_request_context_matches(
        &reviewed,
        "other:7",
        4,
        B256::from([7; 32]),
    ));
    assert!(walletconnect_reviewed_fee_editor_is_current(
        &reviewed,
        3,
        reviewed.selection,
    ));
    assert!(!walletconnect_reviewed_fee_editor_is_current(
        &reviewed,
        4,
        reviewed.selection,
    ));
    assert!(!walletconnect_reviewed_fee_editor_is_current(
        &reviewed,
        3,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 21,
            max_priority_fee_per_gas: 2,
        },
    ));
}

#[test]
fn completion_guard_rejects_changed_lifecycle_fields() {
    let fingerprint = B256::ZERO;
    let state = WalletConnectFeeState {
        request_key: Arc::from("topic:7"),
        review_token: 4,
        payload_fingerprint: fingerprint,
        request_generation: 1,
        dialog_generation: 2,
        editor_generation: 3,
        expiry_timestamp: None,
        status: WalletConnectFeeStatus::Fetching,
        error: None,
        retry_attempt: 0,
        generation: 0,
        simulation_requested: false,
        simulation_retryable: false,
        last_successful_display_projection: None,
    };
    assert!(state.is_completion_current(
        "topic:7",
        4,
        fingerprint,
        state.completion_guard(),
        true,
        1_700_000_000,
    ));
    assert!(!state.is_completion_current(
        "topic:7",
        5,
        fingerprint,
        state.completion_guard(),
        true,
        1_700_000_000,
    ));
    assert!(!state.is_completion_current(
        "other:7",
        4,
        fingerprint,
        state.completion_guard(),
        true,
        1_700_000_000,
    ));
    assert!(!state.is_completion_current(
        "topic:7",
        4,
        B256::from([1; 32]),
        state.completion_guard(),
        true,
        1_700_000_000,
    ));
    assert!(!state.is_completion_current(
        "topic:7",
        4,
        fingerprint,
        state.completion_guard(),
        false,
        1_700_000_000,
    ));
    let mut expired = state;
    expired.expiry_timestamp = Some(1);
    assert!(!expired.is_completion_current(
        "topic:7",
        4,
        fingerprint,
        expired.completion_guard(),
        true,
        2,
    ));
}

#[test]
fn genuine_editor_change_invalidates_old_attempt_and_terminal_retry_is_enabled() {
    let mut state = WalletConnectFeeState {
        request_key: Arc::from("topic:7"),
        review_token: 4,
        payload_fingerprint: B256::ZERO,
        request_generation: 1,
        dialog_generation: 2,
        editor_generation: 3,
        expiry_timestamp: None,
        status: WalletConnectFeeStatus::Fetching,
        error: None,
        retry_attempt: 0,
        generation: 0,
        simulation_requested: false,
        simulation_retryable: false,
        last_successful_display_projection: None,
    };
    let old_guard = state.completion_guard();
    state.invalidate();
    state.editor_generation = 4;
    state.status = WalletConnectFeeStatus::UnavailableFailed;
    assert!(
        !state.is_completion_current("topic:7", 4, B256::ZERO, old_guard, true, 1_700_000_000,)
    );
    assert!(walletconnect_fee_retry_action_enabled(
        state.status,
        false,
        false,
    ));
    assert!(!walletconnect_fee_retry_action_enabled(
        state.status,
        true,
        false,
    ));
    state.begin_manual_retry();
    assert!(walletconnect_fee_retrying(state.retry_attempt, true, false,));
    assert!(!walletconnect_fee_retrying(
        state.retry_attempt,
        false,
        false,
    ));
}

#[test]
fn late_completion_is_discarded_after_navigation_reopen_or_editor_change() {
    let mut state = WalletConnectFeeState {
        request_key: Arc::from("topic:7"),
        review_token: 4,
        payload_fingerprint: B256::ZERO,
        request_generation: 1,
        dialog_generation: 2,
        editor_generation: 3,
        expiry_timestamp: None,
        status: WalletConnectFeeStatus::Fetching,
        error: None,
        retry_attempt: 0,
        generation: 0,
        simulation_requested: false,
        simulation_retryable: false,
        last_successful_display_projection: None,
    };
    let old_guard = state.completion_guard();
    let mut navigation_guard = old_guard;
    navigation_guard.request_generation += 1;
    let mut reopen_guard = old_guard;
    reopen_guard.dialog_generation += 1;
    let mut editor_guard = old_guard;
    editor_guard.editor_generation += 1;
    let mut late_attempt_guard = old_guard;
    late_attempt_guard.generation += 1;
    for guard in [
        navigation_guard,
        reopen_guard,
        editor_guard,
        late_attempt_guard,
    ] {
        assert!(
            !state.is_completion_current("topic:7", 4, B256::ZERO, guard, true, 1_700_000_000,)
        );
    }

    let current_guard = state.begin_attempt(4);
    assert!(
        !state.is_completion_current("topic:7", 4, B256::ZERO, old_guard, true, 1_700_000_000,)
    );
    assert!(state.is_completion_current(
        "topic:7",
        4,
        B256::ZERO,
        current_guard,
        true,
        1_700_000_000,
    ));

    state.invalidate();
    assert!(!state.is_completion_current(
        "topic:7",
        4,
        B256::ZERO,
        current_guard,
        true,
        1_700_000_000,
    ));
    assert!(!state.is_completion_current(
        "topic:7",
        4,
        B256::ZERO,
        current_guard,
        false,
        1_700_000_000,
    ));
}
