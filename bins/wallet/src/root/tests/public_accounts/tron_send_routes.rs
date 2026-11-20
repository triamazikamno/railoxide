use super::*;

#[gpui::test]
fn a_tron_send_whose_operation_route_fails_to_open_reads_and_sends_nothing(
    cx: &mut TestAppContext,
) {
    use crate::root::public_action::PublicActionStepStatus;
    use wallet_ops::settings::{TronEndpointSetting, TronSettings};
    let path = temp_wallet_db_root("tron-send-operation-route");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    // Tron's only endpoint, which nothing may reach.
    let endpoint = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    endpoint.set_nonblocking(true).unwrap();
    let url = format!("http://{}", endpoint.local_addr().unwrap());
    root.update_in(cx, |root, _, _| {
        root.http = root
            .http
            .clone()
            .with_operation_clients_for_tests(|| Err(eyre::eyre!("no operation route")));
        root.tron_public.route = wallet_ops::tron::tron_rpc_route(&TronSettings {
            endpoints: Some(vec![TronEndpointSetting {
                json_rpc_url: url.clone(),
                http_api_url: Some(url.clone()),
                ..TronEndpointSetting::default()
            }]),
            ..TronSettings::default()
        });
    });
    let chain = Arc::new(TronSendChain::new(
        Ok(wallet_ops::tron::http_api::TronBroadcast {
            accepted: true,
            code: None,
        }),
        true,
    ));
    // The stand-in only prepares the intent; the form then reads over Tron's route.
    let intent = open_tron_send(&root, cx, &runtime, &chain);
    root.update_in(cx, |root, window, cx| {
        root.tron_public.send.backend = None;
        root.tron_public
            .send
            .amount_input
            .update(cx, |input, cx| input.set_value("1", window, cx));
        set_tron_test_receiver(root, window, cx);
    });
    start_tron_send_preparation(cx);
    drive_until(cx, &runtime, |cx| {
        root.read_with(cx, |root, _| {
            root.tron_public
                .send
                .form
                .as_ref()
                .is_some_and(|form| !form.preparing)
        })
    });
    root.read_with(cx, |root, _| {
        let form = root.tron_public.send.form.as_ref().unwrap();
        assert!(form.review.is_none());
        assert!(
            form.error
                .as_deref()
                .is_some_and(|error| error.contains("operation route")),
            "{:?}",
            form.error
        );
    });

    approve_tron_send(&root, cx, intent);
    drive_until(cx, &runtime, |cx| {
        !root.read_with(cx, |root, _| root.public_form.sending)
    });
    root.read_with(cx, |root, _| {
        assert_eq!(
            root.public_form.action_progress[0].status,
            PublicActionStepStatus::Error
        );
        assert_eq!(
            root.public_form.action_error_summary.as_deref(),
            Some("The send failed before reaching an endpoint, so nothing was broadcast.")
        );
    });
    assert_eq!(
        endpoint.accept().map(|_| ()).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "a request reached Tron's endpoint"
    );
    assert_eq!(chain.broadcasts(), Vec::<alloy::primitives::B256>::new());

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn tron_settings_that_change_its_route_wait_for_an_active_tron_send(cx: &mut TestAppContext) {
    use crate::root::public_action::PublicActionStepStatus;
    use std::sync::atomic::Ordering::SeqCst;
    let path = temp_wallet_db_root("tron-send-settings");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let chain = Arc::new(TronSendChain::new(
        Ok(wallet_ops::tron::http_api::TronBroadcast {
            accepted: true,
            code: None,
        }),
        true,
    ));
    let intent = open_tron_send(&root, cx, &runtime, &chain);
    // The send waits in its fee read after its signer resolved, before its handoff.
    chain.hold_next_preparation.store(true, SeqCst);
    approve_tron_send(&root, cx, intent);
    drive_until(cx, &runtime, |_| !chain.hold_next_preparation.load(SeqCst));

    let saved = WalletSettings::default();
    assert!(saved.tron.enabled);
    let editor = cx.update(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let store = root.read(cx).vault_store.clone().unwrap();
        let db_path = root.read(cx).options.db_path.clone();
        cx.new(|cx| {
            WalletSettingsEditor::new(
                store,
                &db_path,
                runtime.handle().clone(),
                saved.clone(),
                maintenance,
                None,
                Some(root.downgrade()),
                window,
                cx,
            )
        })
    });
    editor.update(cx, |editor, cx| {
        editor.draft.tron.enabled = false;
        assert!(!editor.persist_draft(cx));
        assert_eq!(
            editor.status.as_deref(),
            Some("Tron has an active send. Wait for it to finish before changing Tron settings.")
        );
        assert!(editor.saved.tron.enabled);
    });

    chain.release_preparation.store(true, SeqCst);
    drive_until(cx, &runtime, |cx| {
        !root.read_with(cx, |root, _| root.public_form.sending)
    });
    root.read_with(cx, |root, _| {
        assert_eq!(
            root.public_form.action_progress[0].status,
            PublicActionStepStatus::Done
        );
    });
    assert_eq!(chain.broadcasts().len(), 1);

    cx.update(|window, _| window.remove_window());
    drop(editor);
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn a_tron_balance_refresh_keeps_the_focused_asset(cx: &mut TestAppContext) {
    use wallet_ops::tron::TRON_CHAIN_ID;
    use wallet_ops::{PublicBalanceAmount, PublicBalanceAsset, PublicBalanceEntry};
    let path = temp_wallet_db_root("tron-refresh-focus");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    root.update_in(cx, |root, window, cx| {
        let store = root.vault_store.clone().unwrap();
        let session = root.view_session.clone().unwrap();
        let account = store
            .import_tron_account(TRON_TEST_PASSWORD, &session, TRON_TEST_KEY, None, false)
            .unwrap();
        root.reload_public_accounts(window, cx);
        root.selected_network = crate::root::SelectedNetwork::Tron;
        root.tron_public.activated = true;
        root.tron_public.selected = Some(Arc::from(account.tron_account_uuid.as_str()));
        // TRX at $0.25 and USDT at $1.
        root.public_broadcaster_anchor_cache.store_native_usd_rate(
            TRON_CHAIN_ID,
            U256::from(250_000),
            6,
        );
        root.public_broadcaster_anchor_cache.store_rate(
            TRON_CHAIN_ID,
            TRON_USDT,
            U256::from(250_000),
        );
        let entry = |id, symbol: &str, amount: u64| PublicBalanceEntry {
            asset: PublicBalanceAsset {
                id,
                symbol: symbol.to_owned(),
                decimals: 6,
            },
            amount: PublicBalanceAmount::Available(U256::from(amount)),
        };
        let balances = |usdt| {
            vec![
                entry(PublicAssetId::Native, "TRX", 100 * TRX_SUN),
                entry(PublicAssetId::Erc20(TRON_USDT), "USDT", usdt),
            ]
        };
        let listed = root
            .listed_account_for_uuid(Some(&account.tron_account_uuid))
            .unwrap()
            .clone();
        root.tron_public.snapshot = Some(Arc::new(wallet_ops::PublicBalanceSnapshot {
            chain_id: TRON_CHAIN_ID,
            refreshed_at: std::time::SystemTime::now(),
            accounts: vec![wallet_ops::PublicAccountBalance {
                account: listed,
                balances: balances(10 * TRX_SUN),
                observed_at: None,
                observed_block: None,
            }],
        }));
        // $25 of TRX sorts before $10 of USDT.
        root.public_form.focused_asset_index = Some(0);
        assert_eq!(root.public_focused_asset(), Some(PublicAssetId::Native));

        // After a refresh $100 of USDT sorts first.
        let scope = root
            .public_balance_scope_for_route(root.tron_public.route.clone().unwrap())
            .unwrap();
        let cache = root.tron_public.balance_cache.clone();
        cache.set_scope(scope.clone());
        let ticket = cache
            .begin_refresh(&scope, [account.tron_account_uuid.clone()])
            .unwrap();
        let read = wallet_ops::tron::balances::TronAccountBalance {
            account,
            balances: balances(100 * TRX_SUN),
            observed_at: std::time::Instant::now(),
            observed_block: 100,
            resources: None,
        };
        root.apply_tron_balance_refresh_result(ticket, Ok(Ok(read)), cx);
        assert_eq!(root.public_form.focused_asset_index, Some(1));
        assert_eq!(root.public_focused_asset(), Some(PublicAssetId::Native));
        root.activate_public_focused_asset(window, cx);
        assert_eq!(
            root.public_form
                .asset_menu
                .as_ref()
                .map(|(asset, _)| *asset),
            Some(PublicAssetId::Native)
        );
    });

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}
