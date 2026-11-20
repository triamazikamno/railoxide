use super::*;

#[gpui::test]
fn public_balance_triggers_follow_autorefresh_and_read_only_their_accounts(
    cx: &mut TestAppContext,
) {
    let path = temp_wallet_db_root("public-refresh-triggers");
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
    let ids = |ids: &[u8]| {
        ids.iter()
            .map(|index| format!("account-{index}"))
            .collect::<std::collections::BTreeSet<_>>()
    };
    root.update_in(cx, |root, window, cx| {
        use_public_only_test_chains(root);
        root.ui_state
            .public_refresh_excluded
            .insert("account-2".into());
        // Chain 10 holds a balance read earlier in the session.
        let scope = root.current_public_balance_scope(10).unwrap();
        let ticket = root
            .public_balance_cache
            .begin_refresh(&scope, ["account-0"])
            .unwrap();
        let read = PublicBalanceSnapshot {
            chain_id: 10,
            refreshed_at: SystemTime::now(),
            accounts: vec![observed_native_balance(&root.public_accounts[0])],
        };
        assert!(
            root.public_balance_cache
                .finish_refresh(ticket, Some(read))
                .accepted
        );

        // Automatic refresh off: showing the tab and a chain round trip read nothing,
        // and returning shows the earlier read.
        root.active_wallet_tab = WalletTab::Private;
        root.select_wallet_tab(WalletTab::Public, cx);
        assert!(root.public_refreshing_accounts().is_empty());
        root.select_chain(20, window, cx);
        assert!(root.public_refreshing_accounts().is_empty());
        assert!(root.public_balance_snapshot.is_none());
        root.select_chain(10, window, cx);
        assert!(root.public_refreshing_accounts().is_empty());
        assert!(
            crate::root::public_balances::public_account_balances_for_chain(
                root.public_balance_snapshot.as_deref(),
                10,
                "account-0",
                PublicAccountStatus::Active,
            )
            .is_some()
        );
        // Hide empty keeps the accounts not yet read on this chain.
        root.ui_state.public_hide_empty_accounts = true;
        assert_eq!(root.public_list_accounts().0.len(), 14);
        root.ui_state.public_hide_empty_accounts = false;

        // Automatic refresh on: the same trigger reads only included active accounts.
        root.ui_state.public_auto_refresh = true;
        root.select_chain(20, window, cx);
        assert_eq!(
            root.public_refreshing_accounts(),
            ids(&[0, 1, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13])
        );

        // Account events read only their account, whatever the toggle says.
        root.ui_state.public_auto_refresh = false;
        root.select_chain(10, window, cx);
        assert!(root.public_refreshing_accounts().is_empty());
        root.refresh_public_action_submitter(10, "account-1", cx);
        assert_eq!(root.public_refreshing_accounts(), ids(&[1]));
        root.public_form.selected_account_uuid = Some(Arc::from("account-3"));
        // The extension shows only its selected account as refreshing.
        let gateway = root.gateway_public_view();
        assert_eq!(gateway.selected_account.as_deref(), Some("account-3"));
        assert!(!gateway.refreshing);
        root.apply_gateway_public_command(
            "peer",
            wallet_ops::gateway::GatewayPublicCommand::RefreshBalances,
            window,
            cx,
        );
        assert_eq!(root.public_refreshing_accounts(), ids(&[1, 3]));
        assert!(root.gateway_public_view().refreshing);
    });

    // A refresh reads one account per ticket and applies each read as it completes. Each
    // read after the first waits a random 0.5-3 s.
    let reads = || runtime.metrics().num_alive_tasks();
    let started = root.update(cx, |root, cx| {
        // A new wallet generation leaves the reads started above behind.
        root.active_wallet_generation += 1;
        root.publish_gateway_desktop_state();
        let scope = root.current_public_balance_scope(10).unwrap();
        let ticket = root
            .public_balance_cache
            .begin_refresh(&scope, ["account-4", "account-5", "account-6"])
            .unwrap();
        let served = [4, 5, 6]
            .into_iter()
            .filter(|index| ticket.includes_account(&format!("account-{index}")))
            .collect::<Vec<u8>>();
        assert_eq!(served.len(), 1);
        // The read returned every requested row; only its own account's is applied.
        let read = PublicBalanceSnapshot {
            chain_id: 10,
            refreshed_at: SystemTime::now(),
            accounts: [4, 5, 6]
                .map(|index| observed_native_balance(&root.public_accounts[index]))
                .into(),
        };
        root.apply_public_balance_refresh_result(ticket, Ok(Ok(read)), None, cx);
        for index in [4, 5, 6] {
            let applied = crate::root::public_balances::public_account_balances_for_chain(
                root.public_balance_snapshot.as_deref(),
                10,
                &format!("account-{index}"),
                PublicAccountStatus::Active,
            )
            .is_some();
            assert_eq!(applied, index == served[0]);
        }
        let others = [4, 5, 6]
            .into_iter()
            .filter(|index| *index != served[0])
            .collect::<Vec<_>>();
        assert_eq!(root.public_refreshing_accounts(), ids(&others));
        reads()
    });
    // Let the follow-up start its wait before the clock moves.
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(400));
    cx.run_until_parked();
    assert_eq!(reads(), started, "the next account waits at least 0.5 s");
    cx.executor().advance_clock(Duration::from_millis(2_700));
    cx.run_until_parked();
    assert_eq!(reads(), started + 1, "then one read starts");

    // A scope change during the wait finishes the follow-up without a read.
    let waiting = root.update(cx, |root, cx| {
        let scope = root.current_public_balance_scope(20).unwrap();
        let ticket = root
            .public_balance_cache
            .begin_refresh(&scope, ["account-4", "account-5"])
            .unwrap();
        root.apply_public_balance_refresh_result(
            ticket,
            Ok(Err(eyre::eyre!("rpc down"))),
            None,
            cx,
        );
        root.active_wallet_generation += 1;
        root.publish_gateway_desktop_state();
        reads()
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(3_100));
    cx.run_until_parked();
    assert_eq!(reads(), waiting);
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn failed_account_refresh_stays_stale_after_another_account_succeeds(cx: &mut TestAppContext) {
    use crate::root::public_balances::{public_account_stale_for_chain, public_section_freshness};
    let path = temp_wallet_db_root("public-refresh-stale");
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
    root.update(cx, |root, cx| {
        use_public_only_test_chains(root);
        let scope = root.current_public_balance_scope(10).unwrap();
        let read = |root: &WalletRoot, indices: &[usize]| PublicBalanceSnapshot {
            chain_id: 10,
            refreshed_at: SystemTime::now(),
            accounts: indices
                .iter()
                .map(|&index| observed_native_balance(&root.public_accounts[index]))
                .collect(),
        };
        let mut refresh =
            |root: &mut WalletRoot,
             id: &str,
             result: Result<PublicBalanceSnapshot, eyre::Report>| {
                let ticket = root
                    .public_balance_cache
                    .begin_refresh(&scope, [id])
                    .unwrap();
                root.apply_public_balance_refresh_result(ticket, Ok(result), None, cx);
            };
        // An Inactive-only read leaves every Active account unread.
        let inactive = read(root, &[14]);
        refresh(root, "account-14", Ok(inactive));
        assert_eq!(
            root.public_summary_status(),
            ("Not fetched".to_owned(), false)
        );
        // A never-read account's failed first refresh is the only warning, and it holds
        // until that account itself is read.
        refresh(root, "account-0", Err(eyre::eyre!("rpc down")));
        let (status, marked) = root.public_summary_status();
        assert!(marked && status.contains("rpc down"), "{status}");
        let first = read(root, &[1]);
        refresh(root, "account-1", Ok(first));
        assert!(root.public_summary_status().1);
        let recovered = read(root, &[0]);
        refresh(root, "account-0", Ok(recovered));
        assert!(!root.public_summary_status().1);

        let ticket = root
            .public_balance_cache
            .begin_refresh(&scope, ["account-0"])
            .unwrap();
        root.apply_public_balance_refresh_result(
            ticket,
            Ok(Err(eyre::eyre!("rpc down"))),
            None,
            cx,
        );
        let second = read(root, &[1]);
        let ticket = root
            .public_balance_cache
            .begin_refresh(&scope, ["account-1"])
            .unwrap();
        root.apply_public_balance_refresh_result(ticket, Ok(Ok(second)), None, cx);

        let snapshot = root.public_balance_snapshot.as_deref();
        let stale =
            |id| public_account_stale_for_chain(snapshot, 10, id, PublicAccountStatus::Active);
        assert!(stale("account-0"));
        assert!(!stale("account-1"));
        let (status, marked) = root.public_summary_status();
        assert!(marked);
        assert!(status.contains("rpc down"), "{status}");
        // The total counts both read accounts and says it leaves out the other twelve.
        let freshness = public_section_freshness(
            snapshot,
            &root.public_accounts,
            10,
            PublicAccountStatus::Active,
            &root.public_failed_accounts(),
        );
        assert_eq!((freshness.counted, freshness.not_refreshed), (2, 12));
        // The extension reports only its selected account's failure, while the desktop
        // strip keeps the aggregate warning.
        root.public_form.selected_account_uuid = Some(Arc::from("account-1"));
        let gateway = root.gateway_public_view();
        assert_eq!(gateway.selected_account.as_deref(), Some("account-1"));
        assert!(!gateway.balance_error);
        assert!(root.public_summary_status().1);
        root.public_form.selected_account_uuid = Some(Arc::from("account-0"));
        let gateway = root.gateway_public_view();
        assert_eq!(gateway.selected_account.as_deref(), Some("account-0"));
        assert!(gateway.balance_error);
    });
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn wallet_view_shortcuts_switch_tabs_and_open_selectors_outside_dialogs(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("wallet-tabs");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let (root, cx) = wallet_view_root(&path, &runtime, cx);
    cx.update(|window, cx| {
        root.read(cx).wallet_focus.clone().focus(window, cx);
        window.draw(cx).clear(cx);
    });
    let active_tab =
        |cx: &gpui::VisualTestContext| root.read_with(cx, |root, _| root.active_wallet_tab);
    assert_eq!(active_tab(cx), WalletTab::Private);
    for (keys, expected) in [
        ("ctrl-tab", WalletTab::Public),
        ("ctrl-tab", WalletTab::Activity),
        ("ctrl-tab", WalletTab::UTXOs),
        ("ctrl-tab", WalletTab::Private),
        ("ctrl-shift-tab", WalletTab::UTXOs),
        ("ctrl-shift-tab", WalletTab::Activity),
        ("ctrl-shift-tab", WalletTab::Public),
    ] {
        cx.simulate_keystrokes(keys);
        assert_eq!(active_tab(cx), expected, "after {keys}");
        // Only the UTXOs tab shows the UTXO table and its controls.
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            cx.debug_bounds("wallet-utxos-body").is_some(),
            expected == WalletTab::UTXOs,
            "UTXO table after {keys}"
        );
    }

    // Header selector shortcuts open the popup with keyboard focus in it.
    cx.update(|window, cx| root.update(cx, |root, cx| root.sync_wallet_select(window, cx)));
    let (wallet_trigger, chain_trigger) = root.read_with(cx, |root, cx| {
        (
            root.wallet_select.read(cx).focus_handle(cx),
            root.chain_select.read(cx).focus_handle(cx),
        )
    });
    let open_selectors = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            let root = root.read(cx);
            let wallet = root.wallet_select.read(cx).focus_handle(cx);
            let chain = root.chain_select.read(cx).focus_handle(cx);
            (
                wallet != wallet_trigger && wallet.is_focused(window),
                chain != chain_trigger && chain.is_focused(window),
            )
        })
    };
    for (keys, expected) in [
        ("ctrl-shift-w", (true, false)),
        ("escape", (false, false)),
        ("ctrl-shift-c", (false, true)),
        ("escape", (false, false)),
    ] {
        cx.simulate_keystrokes(keys);
        assert_eq!(open_selectors(cx), expected, "after {keys}");
    }

    // A modal dialog owns focus, so the wallet view shortcuts must not act behind it.
    cx.update(|window, cx| {
        ui::dialog::WalletDialogExt::open_wallet_dialog(window, cx, |dialog, _, _| dialog);
        window.draw(cx).clear(cx);
    });
    cx.simulate_keystrokes("ctrl-tab");
    assert_eq!(active_tab(cx), WalletTab::Public);
    cx.simulate_keystrokes("ctrl-shift-w ctrl-shift-c");
    assert_eq!(open_selectors(cx), (false, false));
    cx.update(|window, cx| {
        gpui_component::WindowExt::close_dialog(window, cx);
        window.draw(cx).clear(cx);
    });

    // Private and UTXOs are unavailable without a Railgun deployment; Activity isn't.
    cx.update(|_, cx| {
        root.update(cx, |root, cx| {
            root.effective_chain_configs = root
                .effective_chain_configs
                .values()
                .cloned()
                .map(|mut chain| {
                    chain.railgun = None;
                    chain
                })
                .collect();
            cx.notify();
        });
    });
    for (keys, expected) in [
        ("ctrl-tab", WalletTab::Activity),
        ("ctrl-tab", WalletTab::Public),
        ("ctrl-shift-tab", WalletTab::Activity),
        ("ctrl-shift-tab", WalletTab::Public),
    ] {
        cx.simulate_keystrokes(keys);
        assert_eq!(active_tab(cx), expected, "after {keys}");
    }

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn a_wallet_change_keeps_the_open_wallet_tab(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("wallet-change-tab");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let (root, cx) = wallet_view_root(&path, &runtime, cx);
    root.update(cx, |root, cx| {
        root.select_wallet_tab(WalletTab::Activity, cx);
        root.reset_wallet_scoped_state(cx);
    });
    assert_eq!(
        root.read_with(cx, |root, _| root.active_wallet_tab),
        WalletTab::Activity
    );

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}
