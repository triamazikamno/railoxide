use std::sync::Arc;

use alloy::primitives::{Bytes, U256, address, keccak256};
use wallet_ops::vault::PublicAccountSource;
use wallet_ops::{
    NativeShieldPayer, PublicActionGasFeeMode, PublicActionGasFeeQuote,
    PublicActionGasFeeSelection, PublicAssetId, PublicShieldTransactionProfile,
    PublicTransactionIntent,
};

use super::*;
use crate::root::public_action::{
    NativeShieldKey, NativeShieldPreparationState, bind_native_shield_operation,
    native_shield_payer,
};

#[test]
fn advanced_public_send_parser_accepts_calls_creation_and_optional_prefix() {
    let to = address!("0x2222222222222222222222222222222222222222");
    let call = parse_advanced_public_send_intent(
        PublicSendKind::ContractCall,
        &to.to_checksum(None),
        "1.5",
        "0x12345678aa",
        Some(18),
    )
    .expect("advanced call");
    assert_eq!(
        call,
        PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::from(1_500_000_000_000_000_000_u64),
            data: Bytes::from_static(&[0x12, 0x34, 0x56, 0x78, 0xaa]),
        }
    );

    let creation =
        parse_advanced_public_send_intent(PublicSendKind::Deploy, "", "", "60006000", Some(18))
            .expect("advanced creation");
    assert!(matches!(
        creation,
        PublicTransactionIntent::Raw {
            to: None,
            value: U256::ZERO,
            ..
        }
    ));

    let odd = parse_advanced_public_send_intent(
        PublicSendKind::ContractCall,
        &to.to_checksum(None),
        "",
        "0x123",
        Some(18),
    )
    .expect_err("odd hex must fail");
    assert_eq!(odd.field, AdvancedPublicSendField::Data);
    let invalid_to = parse_advanced_public_send_intent(
        PublicSendKind::ContractCall,
        "not-an-address",
        "1",
        "",
        Some(18),
    )
    .expect_err("invalid destination must fail");
    assert_eq!(invalid_to.field, AdvancedPublicSendField::Destination);

    let missing_to =
        parse_advanced_public_send_intent(PublicSendKind::ContractCall, "", "", "", Some(18))
            .expect_err("contract call destination must be explicit");
    assert_eq!(missing_to.field, AdvancedPublicSendField::Destination);

    let missing_init_code = parse_advanced_public_send_intent(
        PublicSendKind::Deploy,
        &to.to_checksum(None),
        "",
        "",
        Some(18),
    )
    .expect_err("deploy init code must be explicit");
    assert_eq!(missing_init_code.field, AdvancedPublicSendField::Data);
}

#[test]
fn advanced_public_send_metadata_and_count_formatting_are_deterministic() {
    let to = address!("0x2222222222222222222222222222222222222222");
    let data = Bytes::from_static(&[0x12, 0x34, 0x56, 0x78, 0xaa]);
    let metadata = advanced_public_send_review_metadata(&PublicTransactionIntent::Raw {
        to: Some(to),
        value: U256::from(7_u64),
        data: data.clone(),
    })
    .expect("advanced metadata");

    assert_eq!(metadata.action_type, "Contract call");
    assert_eq!(metadata.destination, to.to_checksum(None));
    assert_eq!(metadata.data_length, 5);
    assert_eq!(metadata.selector.as_deref(), Some("0x12345678"));
    assert_eq!(
        metadata.data_hash,
        alloy::hex::encode_prefixed(keccak256(&data))
    );
    assert_eq!(metadata.full_data, "0x12345678aa");
    assert_eq!(format_advanced_data_length(1), "1 byte");
    assert_eq!(format_advanced_data_length(2), "2 bytes");
    assert_eq!(format_gas_limit(999), "999");
    assert_eq!(format_gas_limit(1_000), "1,000");
}

#[test]
fn advanced_public_send_hardware_warning_is_added() {
    let software_warnings = advanced_public_send_warnings(PublicAccountSource::Derived);
    assert_eq!(software_warnings.len(), 1);
    assert!(software_warnings[0].contains("Arbitrary transaction data"));

    let hardware_warnings = advanced_public_send_warnings(PublicAccountSource::HardwareDerived);
    assert_eq!(hardware_warnings.len(), 2);
    assert!(hardware_warnings[1].contains("blind signing"));
}

#[test]
fn railway_bnb_auto_authorization_binds_rpc_gas_price_as_legacy_fee() {
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: None,
        rpc_gas_price: 7,
        current_base_fee_per_gas: Some(5),
        suggested_max_fee_per_gas: 12,
        suggested_max_priority_fee_per_gas: 2,
    };
    assert_eq!(
        authorized_public_action_gas_fee_selection(
            PublicActionGasFeeSelection::Auto,
            Some(quote),
            PublicShieldTransactionProfile::Railway,
            PublicAssetId::Erc20(address!("0x3333333333333333333333333333333333333333")),
            56,
        )
        .expect("Railway BNB authorization fee"),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 7,
            max_priority_fee_per_gas: 0,
        }
    );
    assert_eq!(
        authorized_public_action_gas_fee_selection(
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 11,
                max_priority_fee_per_gas: 3,
            },
            Some(quote),
            PublicShieldTransactionProfile::Railway,
            PublicAssetId::Erc20(address!("0x3333333333333333333333333333333333333333")),
            56,
        )
        .expect("Railway BNB custom authorization fee"),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 11,
            max_priority_fee_per_gas: 3,
        }
    );
    assert_eq!(
        authorized_public_action_gas_fee_selection(
            PublicActionGasFeeSelection::Auto,
            Some(quote),
            PublicShieldTransactionProfile::Railway,
            PublicAssetId::Erc20(address!("0x3333333333333333333333333333333333333333")),
            1,
        )
        .expect("Railway type-2 authorization fee"),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 12,
            max_priority_fee_per_gas: 2,
        }
    );
}

#[test]
fn railway_authorization_ceiling_applies_only_to_erc20_shield_auto() {
    let token = address!("0x3333333333333333333333333333333333333333");
    assert!(!public_action_uses_railway_authorization_ceiling(
        PublicActionMode::Shield,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Native,
        PublicActionGasFeeMode::Auto,
    ));
    assert!(public_action_uses_railway_authorization_ceiling(
        PublicActionMode::Shield,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Erc20(token),
        PublicActionGasFeeMode::Auto,
    ));
    assert!(!public_action_uses_railway_authorization_ceiling(
        PublicActionMode::Shield,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Erc20(token),
        PublicActionGasFeeMode::Custom,
    ));
    assert!(!public_action_uses_railway_authorization_ceiling(
        PublicActionMode::Send,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Erc20(token),
        PublicActionGasFeeMode::Auto,
    ));
    assert!(!public_action_uses_railway_authorization_ceiling(
        PublicActionMode::Shield,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Erc20(token),
        PublicActionGasFeeMode::Auto,
    ));
}

#[test]
fn explicit_review_summaries_cannot_use_cached_password() {
    let ordinary = SpendAuthorizationSummary::new("Send", "Review", Vec::new());
    assert!(spend_authorization_can_use_cached_password(&ordinary));

    let advanced = ordinary.requiring_explicit_review();
    assert!(!spend_authorization_can_use_cached_password(&advanced));
}

#[test]
fn stale_native_shield_preparation_cannot_establish_readiness() {
    let key_in = |profile, amount: u64| NativeShieldKey {
        wallet_id: Some(Arc::from("wallet")),
        chain_id: 56,
        public_account_uuid: Arc::from("account"),
        profile,
        amount: U256::from(amount),
    };
    let key = |amount| key_in(PublicShieldTransactionProfile::Railoxide, amount);
    let mut preparation = NativeShieldPreparationState::<u64>::default();
    let earlier = preparation.begin();
    let current = preparation.begin();
    assert!(!preparation.complete(earlier, key(1), Some(&key(1)), Ok(1)));
    assert!(preparation.prepared(Some(&key(1))).is_none());
    // The latest request still cannot serve inputs edited after it started.
    assert!(!preparation.complete(current, key(2), Some(&key(3)), Ok(2)));
    assert!(preparation.complete(current, key(2), Some(&key(2)), Ok(2)));
    assert_eq!(preparation.prepared(Some(&key(2))), Some(&2));
    assert!(preparation.prepared(Some(&key(3))).is_none());
    // Toggling Mimic Railway changes the gas buffer and fee bounds, so it prepares again.
    let mimic = key_in(PublicShieldTransactionProfile::Railway, 2);
    assert!(preparation.prepared(Some(&mimic)).is_none());
    // Preparing again in the other profile is the same action, with the same stealth account.
    let mut bound = None;
    let operation = bind_native_shield_operation(&mut bound, key(2)).unwrap();
    assert_eq!(
        bind_native_shield_operation(&mut bound, mimic).unwrap(),
        operation
    );
    assert_ne!(
        bind_native_shield_operation(&mut bound, key(3)).unwrap(),
        operation
    );
    preparation.invalidate();
    assert!(preparation.prepared(Some(&key(2))).is_none());
}

#[test]
fn native_shield_payer_follows_the_account_source() {
    let executor = serde_json::from_value(serde_json::json!({
        "chain_id": 1,
        "index": 5,
        "operation": wallet_ops::vault::ExecutorOperationId::random().unwrap(),
    }))
    .unwrap();
    for (source, wallet_is_hardware, payer) in [
        (
            PublicAccountSource::HardwareDerived,
            true,
            NativeShieldPayer::Hardware,
        ),
        // A Ledger or Trezor wallet approves its stealth account with a software payer only.
        (
            PublicAccountSource::Imported,
            true,
            NativeShieldPayer::Software,
        ),
        (
            PublicAccountSource::ExecutorDerived(executor),
            true,
            NativeShieldPayer::HardwareWallet,
        ),
        (
            PublicAccountSource::ExecutorDerived(executor),
            false,
            NativeShieldPayer::Software,
        ),
    ] {
        assert_eq!(native_shield_payer(source, wallet_is_hardware), payer);
    }
}

/// A wallet root in a test window, with chain 1's only endpoint at `endpoint` in `roles`.
fn operation_test_root<'a>(
    cx: &'a mut gpui::TestAppContext,
    path: &std::path::Path,
    runtime: &tokio::runtime::Runtime,
    endpoint: reqwest::Url,
    roles: wallet_ops::RpcEndpointRoles,
) -> (gpui::Entity<WalletRoot>, &'a mut gpui::VisualTestContext) {
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = super::public_accounts::fixture_root(path, runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    root.update(cx, |root, _| {
        let mut chain = root.effective_chain_configs.get(1).unwrap().clone();
        chain.rpc_route = wallet_ops::RpcChainRoute::new_with_transports(
            1,
            vec![(endpoint.into(), wallet_ops::RpcTransport::Wallet, roles)],
        );
        root.effective_chain_configs = std::iter::once(chain).collect();
    });
    (root, cx)
}

/// Direct-mode wallet HTTP that counts every operation client it opens.
fn counted_operation_http(opens: &Arc<std::sync::atomic::AtomicUsize>) -> wallet_ops::HttpContext {
    let opens = Arc::clone(opens);
    wallet_ops::HttpContext::with_rpc_client_for_tests(
        reqwest::Client::new(),
        wallet_ops::WalletNetworkMode::Direct,
    )
    .with_operation_clients_for_tests(move || {
        opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(wallet_ops::OperationHttpClient::for_tests(
            reqwest::Client::new(),
            wallet_ops::OperationNetworkIsolation::Unavailable(
                wallet_ops::WalletNetworkMode::Direct,
            ),
        ))
    })
}

const ALL_ROLES: wallet_ops::RpcEndpointRoles = wallet_ops::RpcEndpointRoles {
    general: true,
    sensitive: true,
    submit: true,
};

/// Fills the open Public dialog with an advanced contract call from the selected account.
fn enter_contract_call(
    root: &mut WalletRoot,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<'_, WalletRoot>,
) {
    root.set_public_action_mode(PublicActionMode::Send, window, cx);
    root.set_public_send_kind(PublicSendKind::ContractCall, window, cx);
    root.public_form
        .advanced_send_to_input
        .update(cx, |input, cx| {
            input.set_value("0x1111111111111111111111111111111111111111", window, cx);
        });
    root.public_form
        .advanced_send_data_input
        .update(cx, |input, cx| input.set_value("0x01", window, cx));
}

/// Authorizes and submits the dialog's advanced send against an admitted estimate, then waits
/// for the submission to end. The fixture accounts have no signer, so nothing is broadcast.
fn send_contract_call(
    root: &gpui::Entity<WalletRoot>,
    cx: &mut gpui::VisualTestContext,
    runtime: &tokio::runtime::Runtime,
) {
    root.update_in(cx, |root, window, cx| {
        root.public_form.advanced_send_estimate =
            Some(wallet_ops::PublicAdvancedTransactionEstimate {
                payload_fingerprint: alloy::primitives::B256::ZERO,
                raw_gas_limit: 50_000,
                gas_limit: 60_000,
                max_fee_per_gas: 2,
                max_priority_fee_per_gas: 1,
                expected_fee_per_gas: 2,
                expected_gas_cost: U256::from(100_000),
                max_gas_cost: U256::from(120_000),
            });
        let draft = root.public_send_draft(cx).expect("advanced send draft");
        root.submit_public_send_authorized(
            draft,
            wallet_ops::DesktopPrivateSpendAuthorization::VaultPassword(zeroize::Zeroizing::new(
                "public list test password".to_owned(),
            )),
            window,
            cx,
        );
        assert!(root.public_form.sending);
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while root.read_with(cx, |root, _| root.public_form.sending) {
        assert!(std::time::Instant::now() < deadline, "the send did not end");
        runtime.block_on(tokio::time::sleep(Duration::from_millis(10)));
        cx.run_until_parked();
    }
}

#[gpui::test]
fn public_dialog_estimate_and_send_share_a_client_per_acting_account(
    cx: &mut gpui::TestAppContext,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let path = temp_wallet_db_root("public-action-operation");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let (root, cx) = operation_test_root(
        cx,
        &path,
        &runtime,
        reqwest::Url::parse("http://127.0.0.1:1").unwrap(),
        ALL_ROLES,
    );
    let opens = Arc::new(AtomicUsize::new(0));
    let http = counted_operation_http(&opens);

    // The estimate opens the dialog's client for account 0.
    root.update_in(cx, |root, window, cx| {
        root.http = http.clone();
        root.open_public_action_dialog(Arc::from("account-0"), PublicAssetId::Native, window, cx);
        enter_contract_call(root, window, cx);
        root.estimate_advanced_public_send(cx);
    });
    cx.run_until_parked();
    assert_eq!(opens.load(Ordering::SeqCst), 1);

    // Switching the open dialog to account 1 replaces the client before that account's send.
    root.update_in(cx, |root, window, cx| {
        root.set_public_selected_balance(Arc::from("account-1"), PublicAssetId::Native, window, cx);
        enter_contract_call(root, window, cx);
    });
    send_contract_call(&root, cx, &runtime);
    assert_eq!(opens.load(Ordering::SeqCst), 2);

    // A send uses the client its estimate opened.
    root.update_in(cx, |root, window, cx| {
        root.open_public_action_dialog(Arc::from("account-1"), PublicAssetId::Native, window, cx);
        enter_contract_call(root, window, cx);
        root.estimate_advanced_public_send(cx);
    });
    cx.run_until_parked();
    assert_eq!(opens.load(Ordering::SeqCst), 3);
    send_contract_call(&root, cx, &runtime);
    assert_eq!(opens.load(Ordering::SeqCst), 3);

    // Locking the vault resets the wallet runtime, which releases the dialog's client.
    root.update_in(cx, |root, window, cx| {
        assert!(root.public_form.operation.is_some());
        root.reset_public_wallet_state(window, cx);
        assert!(root.public_form.operation.is_none());
    });

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn public_action_without_an_operation_client_fails_without_the_shared_route(
    cx: &mut gpui::TestAppContext,
) {
    use std::io::Read as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let path = temp_wallet_db_root("public-action-no-operation-client");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    // Chain 1's only Sensitive and Submit endpoint counts every connection it receives.
    let connections = Arc::new(AtomicUsize::new(0));
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let endpoint =
        reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let counted = Arc::clone(&connections);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = stream.read(&mut [0; 1024]);
        }
    });
    let (root, cx) = operation_test_root(
        cx,
        &path,
        &runtime,
        endpoint,
        wallet_ops::RpcEndpointRoles {
            general: false,
            ..ALL_ROLES
        },
    );

    // Built-in Tor without the internal SOCKS bridge can't open an operation client.
    let shared = wallet_ops::HttpContext::with_rpc_client_for_tests(
        reqwest::Client::new(),
        wallet_ops::WalletNetworkMode::Tor,
    );
    root.update_in(cx, |root, window, cx| {
        root.http = shared.clone();
        root.open_public_action_dialog(Arc::from("account-0"), PublicAssetId::Native, window, cx);
        enter_contract_call(root, window, cx);
        root.estimate_advanced_public_send(cx);
    });
    cx.run_until_parked();
    let estimate_error = root.read_with(cx, |root, _| root.public_form.send_error.clone());
    assert!(
        estimate_error
            .as_deref()
            .is_some_and(|error| error.contains("internal SOCKS bridge")),
        "{estimate_error:?}"
    );

    send_contract_call(&root, cx, &runtime);
    let send_error = root.read_with(cx, |root, _| root.public_form.send_error.clone());
    assert!(
        send_error
            .as_deref()
            .is_some_and(|error| error.contains("internal SOCKS bridge")),
        "{send_error:?}"
    );
    assert_eq!(connections.load(Ordering::SeqCst), 0);

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}
