use super::*;

#[gpui::test]
fn pinned_form_keeps_its_pin_until_the_user_accepts_leaving_it(cx: &mut gpui::TestAppContext) {
    let path = std::env::temp_dir().join(format!(
        "pinned-private-form-{:032x}",
        rand::random::<u128>()
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let wallet = crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let asset = UnshieldAsset {
        chain_id: 1,
        token: Address::repeat_byte(0x55),
        label: "TEST".into(),
        decimals: Some(18),
        total: U256::from(1_000),
        poi_verified_total: U256::from(1_000),
        max_batched: U256::from(1_000),
        icon_path: None,
    };
    let key = UnshieldAssetKey::from_asset(&asset);
    let pin = wallet_ops::PublicBroadcasterInputPin {
        inputs: vec![wallet_ops::SelectedInputIdentity {
            tree: 0,
            position: 7,
        }],
        overridden: false,
    };
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.initialize_send_form(asset, window, cx);
            root.send_forms.get_mut(&key).unwrap().input_pin = Some(pin.clone());
            let amount = root.send_forms[&key].amount_input.clone();
            amount.update(cx, |input, cx| input.set_value("0.5", window, cx));
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            assert_eq!(root.send_forms[&key].input_pin, Some(pin.clone()));
            root.set_send_delivery_mode(key, DeliveryMode::ManualCalldata, window, cx);
            assert!(window.has_active_dialog(cx), "leaving the pin asks first");
            assert_eq!(
                root.send_forms[&key].delivery_mode,
                DeliveryMode::PublicBroadcaster
            );
        });
        // Cancel leaves the form as it was.
        window.close_all_dialogs(cx);
        root.update(cx, |root, cx| {
            assert_eq!(
                root.send_forms[&key].delivery_mode,
                DeliveryMode::PublicBroadcaster
            );
            assert_eq!(root.send_forms[&key].input_pin, Some(pin.clone()));
            root.set_send_delivery_mode(key, DeliveryMode::ManualCalldata, window, cx);
        });
        window.draw(cx).clear(cx);
    });
    cx.run_until_parked();
    cx.dispatch_action(gpui_component::dialog::Confirm { secondary: false });
    cx.run_until_parked();
    root.read_with(cx, |root, _| {
        let form = &root.send_forms[&key];
        assert_eq!(form.delivery_mode, DeliveryMode::ManualCalldata);
        assert_eq!(form.input_pin, None, "accepting the risk clears the pin");
    });
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    drop(entered);
    drop(runtime);
    std::fs::remove_dir_all(path).unwrap();
}

/// Clicks Generate, presses Enter in the amount and clicks the estimate Retry of a `kind` form
/// on a ready chain, and checks that each reaches that form's handler.
fn form_controls_reach_their_own_form(kind: DeliveryFormKind, cx: &mut gpui::TestAppContext) {
    let path = std::env::temp_dir().join(format!(
        "private-form-routing-{:032x}",
        rand::random::<u128>()
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let wallet = crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let (session, sessions) = start_ready_chain(&root, &runtime, cx);
    let asset = UnshieldAsset {
        chain_id: 1,
        token: Address::repeat_byte(0x55),
        label: "TEST".into(),
        decimals: Some(18),
        total: U256::from(1_000),
        poi_verified_total: U256::from(1_000),
        max_batched: U256::from(1_000),
        icon_path: None,
    };
    let key = UnshieldAssetKey::from_asset(&asset);
    root.update_in(cx, |root, window, cx| match kind {
        DeliveryFormKind::Send => root.open_send_form(asset, window, cx),
        DeliveryFormKind::Unshield => root.open_unshield_form(asset, window, cx),
    });
    assert_controls_reach_form(kind, key, &root, cx);

    cx.update(|window, _| window.remove_window());
    runtime.block_on(async {
        session.stop().await.unwrap();
        sessions.shutdown().await;
    });
    drop((host, root, session, sessions));
    cx.run_until_parked();
    drop(entered);
    drop(runtime);
    std::fs::remove_dir_all(path).unwrap();
}

/// Starts chain 1's session against a closed RPC port and marks the chain ready, which enables
/// Generate.
fn start_ready_chain(
    root: &Entity<WalletRoot>,
    runtime: &tokio::runtime::Runtime,
    cx: &mut gpui::VisualTestContext,
) -> (
    Arc<wallet_ops::WalletSession>,
    wallet_ops::WalletSessionStore,
) {
    let rpc_url = reqwest::Url::parse("http://127.0.0.1:1").unwrap();
    let poi = wallet_ops::PoiReadSource::PoiProxy {
        rpc_url: rpc_url.clone().into(),
    };
    let (sessions, request, http) = root.read_with(cx, |root, _| {
        let mut chain = root.effective_chain_configs.get(1).unwrap().clone();
        chain.rpc_route = wallet_ops::RpcChainRoute::new(1, vec![rpc_url]);
        let private = chain.railgun.as_mut().unwrap();
        private.archive_rpc_url = None;
        private.sync.quick_sync_endpoint = None;
        private.sync.indexed_artifact_source = None;
        let sessions = wallet_ops::WalletSessionStore::from_db(
            root.vault_store.as_ref().unwrap().db(),
            poi.clone(),
        )
        .unwrap();
        let request = wallet_ops::ViewWalletChainSessionRequest {
            view_session: root.view_session.clone().unwrap(),
            wallet_scope_generation: 0,
            chain_id: 1,
            effective_chain: chain,
            sync_start_policy: wallet_ops::DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
            init_block_number: Some(0),
            sync_to_block: Some(0),
            use_indexed_wallet_catch_up: false,
            poi_read_source: poi,
            rewind_wallet_cache: false,
            progress_tx: None,
        };
        (sessions, request, root.http.clone())
    });
    let session = Arc::new(
        runtime
            .block_on(sessions.start_view_wallet_session_immediate(request, &http))
            .unwrap(),
    );
    let observation = session.observation_rx.borrow().clone();
    root.update(cx, |root, _| {
        root.chain_states.insert(
            1,
            ChainUtxoState::Ready {
                snapshot: observation.snapshot,
                session: Arc::clone(&session),
                observer_token: root.wallet_sync_lifecycle.prepare_startup(1).observer_token,
                sync_tip: *session.sync_tip_rx.borrow(),
                poi_refreshing: false,
                ppoi_workflow_status: observation.ppoi_workflow_status,
            },
        );
    });
    (session, sessions)
}

/// Clicks Generate, presses Enter in the amount and clicks the estimate Retry of the open `kind`
/// form of `key`.
fn assert_controls_reach_form(
    kind: DeliveryFormKind,
    key: UnshieldAssetKey,
    root: &Entity<WalletRoot>,
    cx: &mut gpui::VisualTestContext,
) {
    // Only the `kind` form exists, so a control routed to the other kind's handler finds no
    // form, changes nothing, and fails its assertion.
    macro_rules! form {
        ($root:expr, |$form:ident| $body:expr) => {
            match kind {
                DeliveryFormKind::Send => {
                    let $form = $root.send_forms.get_mut(&key).unwrap();
                    $body
                }
                DeliveryFormKind::Unshield => {
                    let $form = $root.unshield_forms.get_mut(&key).unwrap();
                    $body
                }
            }
        };
    }
    let (generate, retry) = match kind {
        DeliveryFormKind::Send => ("send-generate", "send-estimate-retry"),
        DeliveryFormKind::Unshield => ("unshield-generate", "unshield-estimate-retry"),
    };
    let draw = |cx: &mut gpui::VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));
    let click = |cx: &mut gpui::VisualTestContext, selector: &'static str| {
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    };
    let has_error = |cx: &mut gpui::VisualTestContext| {
        root.update(cx, |root, _| form!(root, |form| form.error.is_some()))
    };
    root.update(cx, |root, cx| {
        form!(root, |form| form.delivery_mode =
            DeliveryMode::ManualCalldata);
        cx.notify();
    });
    cx.run_until_parked();
    draw(cx);
    // An empty recipient fails the spend draft with a form error, before any dialog.
    assert!(!has_error(cx));
    click(cx, generate);
    assert!(has_error(cx), "Generate reaches its form");

    root.update_in(cx, |root, window, cx| {
        let amount = form!(root, |form| {
            form.error = None;
            form.amount_input.clone()
        });
        amount.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    });
    draw(cx);
    cx.simulate_keystrokes("enter");
    assert!(has_error(cx), "amount Enter reaches its form");

    root.update(cx, |root, cx| {
        form!(root, |form| {
            form.delivery_mode = DeliveryMode::PublicBroadcaster;
            form.cost_estimate = None;
            form.cost_estimate_pending = false;
            form.estimating_cost = false;
            form.error = Some("stand-in".into());
        });
        cx.notify();
    });
    draw(cx);
    click(cx, retry);
    // Retry restarts the estimate, which clears the error. The blank amount then ends the
    // estimate at once, so its `estimate_id` is back at 0 and can't show the restart.
    assert!(!has_error(cx), "Retry reaches its form");
}

#[gpui::test]
fn send_form_controls_reach_the_send_handlers(cx: &mut gpui::TestAppContext) {
    form_controls_reach_their_own_form(DeliveryFormKind::Send, cx);
}

#[gpui::test]
fn unshield_form_controls_reach_the_unshield_handlers(cx: &mut gpui::TestAppContext) {
    form_controls_reach_their_own_form(DeliveryFormKind::Unshield, cx);
}
