use super::*;

#[gpui::test]
fn selecting_tron_leaves_evm_views_unavailable_and_offers_no_action(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("tron-selection");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let (root, cx) = wallet_view_root(&path, &runtime, cx);
    cx.simulate_resize(gpui::size(gpui::px(1224.), gpui::px(900.)));
    let remembered = root.read_with(cx, |root, _| {
        assert_eq!(root.active_wallet_tab, WalletTab::Private);
        root.ui_state.last_chain_id
    });
    // Tron is chosen from the header's network selector, after the EVM chains.
    cx.update(|window, cx| {
        root.read(cx).wallet_focus.clone().focus(window, cx);
        window.draw(cx).clear(cx);
    });
    cx.simulate_keystrokes("ctrl-shift-c");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let ethereum = cx.debug_bounds("chain-option-1").unwrap();
    let tron = cx.debug_bounds("chain-option-tron").unwrap();
    assert!(tron.top() > ethereum.bottom());
    cx.simulate_click(tron.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        assert_eq!(root.selected_network, SelectedNetwork::Tron);
        assert_eq!(
            root.chain_select.read(cx).selected_value(),
            Some(&SelectedNetwork::Tron)
        );
        assert_eq!(root.last_evm_chain, 1);
        assert_eq!(root.ui_state.last_chain_id, remembered);

        // Private and UTXOs stay unavailable, with Tron's reason. Activity lists Tron's entries.
        assert_eq!(root.active_wallet_tab, WalletTab::Public);
        root.select_wallet_tab(WalletTab::Private, cx);
        root.select_wallet_tab(WalletTab::UTXOs, cx);
        assert_eq!(root.active_wallet_tab, WalletTab::Public);
        assert!(root.railgun_unavailable_reason().contains("Tron"));
        root.select_wallet_tab(WalletTab::Activity, cx);
        assert_eq!(root.active_wallet_tab, WalletTab::Activity);
        root.select_wallet_tab(WalletTab::Public, cx);

        // Shield and swaps have nothing to start from.
        root.public_form.action_mode = PublicActionMode::Send;
        root.set_public_action_mode(PublicActionMode::Shield, window, cx);
        assert_eq!(root.public_form.action_mode, PublicActionMode::Send);
        assert!(root.private_swaps_view().is_none());

        // Governance shows its not-deployed states and reads nothing.
        root.governance.tab = crate::root::governance::GovernanceTab::Staking;
        root.start_staking_refresh(cx);
        assert!(root.governance.staking.key.is_none());
        root.governance.tab = crate::root::governance::GovernanceTab::Proposals;
        root.start_proposals_refresh(true, cx);
        assert!(root.proposals.checked);
        assert!(root.proposals.overview.is_none());
        assert!(!root.proposals.loading);
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));

    // The Public tab lists no EVM account and offers no account action.
    assert!(cx.debug_bounds("public-tron-empty").is_some());
    for selector in ["public-row-account-0", "public-asset-account-0-Native"] {
        assert!(cx.debug_bounds(selector).is_none(), "{selector}");
    }
    // WalletConnect is disabled: its trigger opens nothing.
    let walletconnect = cx.debug_bounds("public-walletconnect").unwrap();
    cx.simulate_click(walletconnect.center(), gpui::Modifiers::default());
    cx.update(|window, cx| {
        assert!(!gpui_component::WindowExt::has_active_dialog(window, cx));
    });

    // Disabling Tron while it is selected returns to the last EVM chain, and Tron can't be
    // selected again.
    root.update_in(cx, |root, window, cx| {
        let mut settings = WalletSettings::default();
        settings.tron.enabled = false;
        root.apply_saved_tron_enabled(&settings, cx);
        assert_eq!(root.selected_network, SelectedNetwork::Evm(1));
        root.select_tron(window, cx);
        assert_eq!(root.selected_network, SelectedNetwork::Evm(1));
    });
    cx.run_until_parked();
    root.read_with(cx, |root, cx| {
        assert_eq!(
            root.chain_select.read(cx).selected_value(),
            Some(&SelectedNetwork::Evm(1))
        );
    });

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn gateway_keeps_reporting_the_last_evm_chain_while_tron_is_selected(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("tron-gateway");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    root.update_in(cx, |root, window, cx| {
        let accounts = |view: &wallet_ops::gateway::GatewayPublicView| {
            view.balances
                .iter()
                .map(|balances| balances.account_uuid.clone())
                .collect::<Vec<_>>()
        };
        let before = root.gateway_public_view();
        root.select_tron(window, cx);
        assert_eq!(root.selected_network, SelectedNetwork::Tron);
        let after = root.gateway_public_view();
        assert_eq!(after.selected_chain, Some(1));
        assert_eq!(after.selected_chain, before.selected_chain);
        assert_ne!(accounts(&after), [] as [String; 0]);
        assert_eq!(accounts(&after), accounts(&before));
        assert_eq!(root.gateway_private_view().selected_chain, Some(1));
    });
    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn native_shield_route_follows_the_payer_and_keeps_the_mimic_preference(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("native-mimic-unavailable");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    root.update(cx, |root, _| {
        root.public_accounts[0].source = PublicAccountSource::HardwareDerived;
        root.public_form.selected_account_uuid = Some(Arc::from(
            root.public_accounts[0].public_account_uuid.as_str(),
        ));
        root.public_form.mimic_railway_shield = true;
        for (asset, mimic) in [
            (PublicAssetId::Native, false),
            (PublicAssetId::Erc20(Address::repeat_byte(42)), true),
        ] {
            root.public_form.selected_asset = Some(asset);
            let profile = if mimic {
                wallet_ops::PublicShieldTransactionProfile::Railway
            } else {
                wallet_ops::PublicShieldTransactionProfile::Railoxide
            };
            assert_eq!(root.mimic_railway_shield_effective(), mimic);
            assert_eq!(
                root.public_action_quote_route(PublicActionMode::Shield),
                (asset, profile)
            );
            assert!(!root.native_shield_stealth_applies(PublicActionMode::Shield));
        }
        assert!(root.public_form.mimic_railway_shield);
        // A hardware payer's native shield wraps, approves and shields in Railoxide.
        root.public_form.selected_asset = Some(PublicAssetId::Native);
        assert!(matches!(
            root.native_shield_route(),
            Some(Ok(wallet_ops::NativeShieldRoute::WrapFallback { .. }))
        ));
        assert!(root.public_shield_may_approve());
        // A software payer takes the stealth route in either profile and signs no approval.
        root.public_accounts[0].source = PublicAccountSource::Derived;
        for mimic in [true, false] {
            root.public_form.mimic_railway_shield = mimic;
            assert_eq!(root.mimic_railway_shield_effective(), mimic);
            assert!(root.native_shield_stealth_applies(PublicActionMode::Shield));
            assert!(!root.public_shield_may_approve());
        }
    });
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn network_status_stays_reachable_above_dialogs(cx: &mut TestAppContext) {
    use gpui::InteractiveElement as _;
    use gpui_component::input::{Input, InputState};

    let path = temp_wallet_db_root("network-status-dialogs");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(|cx| {
        gpui_component::init(cx);
        ui::theme::apply_zenburn_component_theme(cx);
        install_wallet_action_bindings(cx);
    });
    let mut root = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let wallet = fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    cx.simulate_resize(gpui::size(gpui::px(1400.), gpui::px(1000.)));
    // Enough log rows for the logs panel to scroll.
    let logs = ui::logs::LogStore::new(200);
    for ix in 0..200 {
        logs.push(ui::logs::LogEntry {
            seq: 0,
            unix_ms: 0,
            level: tracing::Level::INFO,
            target: "test".into(),
            message: format!("line {ix}").into(),
        });
    }
    // Two stacked dialogs with entered text; the second input has focus.
    let inputs = cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.network_status.network_health.mode = wallet_ops::WalletNetworkMode::Tor;
            root.network_status.network_health.state = wallet_ops::WalletNetworkHealthState::Ready;
            root.logs_panel.logs = cx.new(|cx| ui::logs::LogsPane::new(logs, window, cx));
            cx.notify();
        });
        let inputs = ["first draft", "second draft"]
            .map(|value| cx.new(|cx| InputState::new(window, cx).default_value(value)));
        for (ix, input) in inputs.iter().enumerate() {
            let input = input.clone();
            ui::dialog::WalletDialogExt::open_wallet_dialog(window, cx, move |dialog, _, _| {
                dialog.child(Input::new(&input)).child(
                    div()
                        .debug_selector(move || format!("test-dialog-{ix}"))
                        .child("Dialog"),
                )
            });
        }
        inputs[1].update(cx, |input, cx| input.focus(window, cx));
        window.draw(cx).clear(cx);
        inputs
    });
    let draw = |cx: &mut gpui::VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));
    let click = |cx: &mut gpui::VisualTestContext, position: gpui::Point<gpui::Pixels>| {
        cx.simulate_click(position, gpui::Modifiers::default());
        draw(cx);
    };
    let pill = cx
        .debug_bounds("wallet-network-status-pill")
        .expect("network pill is rendered");
    // (popover open, focus inside popover, dialogs shown, second input focused)
    let state = |cx: &mut gpui::VisualTestContext| {
        let dialogs = ["test-dialog-0", "test-dialog-1"]
            .into_iter()
            .filter(|id| cx.debug_bounds(id).is_some())
            .count();
        cx.update(|window, cx| {
            let root = root.read(cx);
            (
                root.network_status.popover_open,
                root.network_status
                    .popover_focus
                    .contains_focused(window, cx),
                dialogs,
                inputs[1].read(cx).focus_handle(cx).is_focused(window),
            )
        })
    };
    let toggle_from_shortcut = |cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| {
            root.update(cx, WalletRoot::toggle_network_status_from_shortcut);
        });
        draw(cx);
    };

    click(cx, pill.center());
    assert_eq!(
        state(cx),
        (true, true, 2, false),
        "pill opens above dialogs"
    );
    for keys in ["tab", "shift-tab"] {
        cx.simulate_keystrokes(keys);
        draw(cx);
        assert_eq!(state(cx), (true, true, 2, false), "{keys} stays in popover");
    }
    let new_session = cx
        .debug_bounds("wallet-network-new-tor-session")
        .expect("Tor session action is rendered");
    click(cx, new_session.center());
    assert_eq!(
        state(cx),
        (true, true, 2, false),
        "recovery keeps everything open"
    );
    cx.update(|_, cx| {
        assert!(root.read(cx).network_status.error.is_some(), "recovery ran");
        assert_eq!(
            inputs
                .each_ref()
                .map(|input| input.read(cx).value().to_string()),
            ["first draft", "second draft"]
        );
    });

    cx.simulate_keystrokes("escape");
    draw(cx);
    assert_eq!(
        state(cx),
        (false, false, 2, true),
        "Escape closes only popover"
    );
    click(cx, pill.center());
    click(cx, gpui::point(gpui::px(1380.), gpui::px(20.)));
    assert_eq!(
        state(cx),
        (false, false, 2, true),
        "backdrop press closes only popover"
    );

    toggle_from_shortcut(cx);
    assert_eq!(
        state(cx),
        (true, true, 2, false),
        "shortcut opens and focuses"
    );
    toggle_from_shortcut(cx);
    assert_eq!(
        state(cx),
        (false, false, 2, true),
        "shortcut closes only popover"
    );

    // The rest of the lifted bar: nothing in it may close or change the dialogs.
    let dialogs_intact = |cx: &mut gpui::VisualTestContext| {
        state(cx) == (false, false, 2, true)
            && cx.update(|_, cx| {
                inputs
                    .each_ref()
                    .map(|input| input.read(cx).value().to_string())
                    == ["first draft", "second draft"]
            })
    };
    // (logs open, panel rendered)
    let logs_state = |cx: &mut gpui::VisualTestContext| {
        let panel = cx.debug_bounds("wallet-logs-panel").is_some();
        (cx.update(|_, cx| root.read(cx).logs_panel.open), panel)
    };
    let logs_toggle = cx
        .debug_bounds("wallet-status-bar-logs")
        .expect("Logs toggle is rendered");
    click(cx, logs_toggle.center());
    assert_eq!(logs_state(cx), (true, true), "Logs opens the panel");
    assert!(dialogs_intact(cx), "opening logs keeps the dialogs");

    let panel = cx
        .debug_bounds("wallet-logs-panel")
        .expect("logs panel is rendered");
    let pane = cx.update(|_, cx| root.read(cx).logs_panel.logs.clone());
    let pane_notified = std::rc::Rc::new(std::cell::Cell::new(false));
    let _pane_observer = cx.update(|_, cx| {
        let pane_notified = pane_notified.clone();
        cx.observe(&pane, move |_, _| pane_notified.set(true))
    });
    cx.simulate_event(gpui::ScrollEvent {
        position: panel.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(200.))),
        ..Default::default()
    });
    draw(cx);
    // The pane notifies only from its list's scroll handler here.
    assert!(pane_notified.get(), "wheel scrolls the lifted panel");
    assert!(dialogs_intact(cx), "scrolling keeps the dialogs");

    click(
        cx,
        gpui::point(panel.center().x, panel.top() + gpui::px(2.)),
    );
    assert!(dialogs_intact(cx), "resize edge press keeps dialog focus");
    let title_bar = cx
        .debug_bounds("wallet-logs-title-bar")
        .expect("logs title bar is rendered");
    click(
        cx,
        gpui::point(title_bar.left() + gpui::px(4.), title_bar.center().y),
    );
    assert!(dialogs_intact(cx), "title bar press keeps dialog focus");
    // The center is a log row, below the header and the top resize handle. It takes focus,
    // and closing the panel gives focus back to the dialog.
    click(cx, panel.center());
    assert!(!dialogs_intact(cx), "row press takes focus");
    click(cx, logs_toggle.center());
    assert_eq!(logs_state(cx), (false, false), "Logs closes the panel");
    assert!(dialogs_intact(cx), "closing logs keeps the dialogs");

    let pairing = cx
        .debug_bounds("wallet-gateway-status-icon")
        .expect("pairing icon is rendered");
    click(cx, pairing.center());
    assert!(cx.update(|_, cx| !root.read(cx).gateway.popover_open));
    assert!(dialogs_intact(cx), "pairing icon is inert above dialogs");

    let bar = cx
        .debug_bounds("wallet-status-bar")
        .expect("status bar is rendered");
    click(cx, bar.center());
    assert!(
        dialogs_intact(cx),
        "empty bar press reaches nothing beneath"
    );

    cx.update(|_, cx| {
        root.update(cx, |root, cx| {
            root.prover_cache_build.progress =
                Some(wallet_ops::ProverCacheBuildProgress::preparing());
            cx.notify();
        });
    });
    draw(cx);
    let prover = cx
        .debug_bounds("wallet-prover-cache-build-pill")
        .expect("prover cache indicator is rendered");
    click(cx, prover.center());
    assert!(cx.update(|_, cx| !root.read(cx).prover_cache_build.popover_open));
    assert!(
        dialogs_intact(cx),
        "prover cache indicator is inert above dialogs"
    );

    cx.update(|_, cx| {
        root.update(cx, |root, cx| {
            let chain_id = root.selected_evm_chain().expect("EVM chain is selected");
            root.chain_states.insert(
                chain_id,
                ChainUtxoState::Loading {
                    progress: crate::root::chain_load::ChainSyncProgress::default(),
                },
            );
            cx.notify();
        });
    });
    draw(cx);
    let ppoi = cx
        .debug_bounds("wallet-status-ppoi")
        .expect("PPOI chip is rendered");
    click(cx, ppoi.center());
    assert!(cx.update(|_, cx| !root.read(cx).private_pending_status_dialog_open));
    assert!(dialogs_intact(cx), "PPOI chip is inert above dialogs");

    toggle_from_shortcut(cx);
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.lock_vault(window, cx);
            assert!(!root.network_status.popover_open, "lock closes popover");
            root.toggle_network_status_from_shortcut(cx);
            assert!(
                !root.network_status.popover_open,
                "shortcut ignored while locked"
            );
        });
    });

    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}
