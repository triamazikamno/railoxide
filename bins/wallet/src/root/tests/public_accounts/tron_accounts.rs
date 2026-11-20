use super::*;

fn leaked_selector(selector: String) -> &'static str {
    Box::leak(selector.into_boxed_str())
}

fn set_input(
    input: &Entity<gpui_component::input::InputState>,
    value: &str,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) {
    input.update(cx, |input, cx| input.set_value(value, window, cx));
}

#[gpui::test]
fn tron_accounts_share_the_public_list_sections_search_sort_and_lifecycle(cx: &mut TestAppContext) {
    use wallet_ops::vault::TronAccountSource;
    let path = temp_wallet_db_root("tron-list");
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
    let (host, cx) = cx.add_window_view(|window, cx| {
        let root = fixture_root(&path, &runtime, window, cx);
        let view = cx.new(|cx| {
            cx.observe(&root, |_, _, cx| cx.notify()).detach();
            PublicListWindow(root)
        });
        Root::new(view, window, cx)
    });
    let root = host.read_with(cx, |host, cx| {
        host.view()
            .clone()
            .downcast::<PublicListWindow>()
            .unwrap()
            .read(cx)
            .0
            .clone()
    });
    cx.simulate_resize(gpui::size(gpui::px(1224.), gpui::px(900.)));
    let labels = |accounts: &[PublicAccountMetadata]| {
        accounts
            .iter()
            .map(|account| account.label.clone().unwrap_or_default())
            .collect::<Vec<_>>()
    };
    let (derived, imported, evm_rows) = root.update_in(cx, |root, window, cx| {
        let store = root.vault_store.clone().unwrap();
        let session = root.view_session.clone().unwrap();
        let derived = store
            .add_derived_tron_account(TRON_TEST_PASSWORD, &session, Some("Savings"), None)
            .unwrap();
        let imported = store
            .import_tron_account(
                TRON_TEST_PASSWORD,
                &session,
                TRON_TEST_KEY,
                Some("Cold key"),
                false,
            )
            .unwrap();
        root.reload_public_accounts(window, cx);
        root.select_tron(window, cx);
        let evm_rows = root
            .public_accounts
            .iter()
            .map(|account| leaked_selector(format!("public-row-{}", account.public_account_uuid)))
            .collect::<Vec<_>>();
        assert_ne!(evm_rows, [] as [&str; 0]);

        // Sort is shared with EVM accounts.
        root.ui_state.public_account_sort = wallet_ops::settings::PublicAccountSort::Name;
        assert_eq!(
            labels(&root.public_list_accounts().0),
            ["Cold key", "Savings"]
        );
        root.ui_state.public_account_sort = wallet_ops::settings::PublicAccountSort::Added;
        assert_eq!(
            labels(&root.public_list_accounts().0),
            ["Savings", "Cold key"]
        );

        // Search matches base58 addresses.
        let base58 = wallet_ops::tron::address::encode_tron_address(imported.address);
        root.public_form.search_query = Arc::from(&base58[3..12]);
        assert_eq!(labels(&root.public_list_accounts().0), ["Cold key"]);
        root.public_form.search_query = Arc::from("");
        (derived, imported, evm_rows)
    });

    // The rows are Tron accounts only.
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for account in [&derived, &imported] {
        let row = leaked_selector(format!("public-row-{}", account.tron_account_uuid));
        assert!(cx.debug_bounds(row).is_some(), "{row}");
    }
    for row in evm_rows {
        assert!(cx.debug_bounds(row).is_none(), "{row}");
    }

    root.update_in(cx, |root, window, cx| {
        let evm_before = root.public_accounts.clone();
        // Hiding a derived account moves it to Inactive, and the next Add skips its index.
        root.deactivate_public_account(&derived.tron_account_uuid, window, cx);
        let (active, inactive) = root.public_list_accounts();
        assert_eq!(labels(&active), ["Cold key"]);
        assert_eq!(labels(&inactive), ["Savings"]);
        let input = root.public_form.add_password_input.clone();
        set_input(&input, TRON_TEST_PASSWORD, window, cx);
        root.add_public_derived_account_from_input(window, cx);
        assert_eq!(root.public_form.error, None);
        let sources = root
            .tron_public
            .accounts
            .iter()
            .map(|account| account.source)
            .collect::<Vec<_>>();
        assert!(sources.contains(&TronAccountSource::Derived { index: 0 }));
        assert!(sources.contains(&TronAccountSource::Derived { index: 1 }));

        // Deleting the imported account removes it.
        root.delete_public_account(&imported.tron_account_uuid, window, cx);
        assert!(
            root.tron_public
                .accounts
                .iter()
                .all(|account| account.tron_account_uuid != imported.tron_account_uuid)
        );
        assert_eq!(root.public_accounts, evm_before);
    });
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn importing_on_tron_adds_only_a_tron_account_that_receives_at_its_base58_address(
    cx: &mut TestAppContext,
) {
    use crate::root::dialogs::PublicAccountDialogKind;
    let path = temp_wallet_db_root("tron-import");
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
    let (host, cx) = cx.add_window_view(|window, cx| {
        let root = fixture_root(&path, &runtime, window, cx);
        let view = cx.new(|cx| {
            cx.observe(&root, |_, _, cx| cx.notify()).detach();
            PublicListWindow(root)
        });
        Root::new(view, window, cx)
    });
    let root = host.read_with(cx, |host, cx| {
        host.view()
            .clone()
            .downcast::<PublicListWindow>()
            .unwrap()
            .read(cx)
            .0
            .clone()
    });
    let import = |root: &mut WalletRoot, window: &mut Window, cx: &mut Context<'_, WalletRoot>| {
        root.open_public_account_dialog(PublicAccountDialogKind::Import, window, cx);
        assert!(
            root.public_form.import_tron,
            "Import defaults to the selected network"
        );
        let key = root.public_form.import_private_key_input.clone();
        let password = root.public_form.import_password_input.clone();
        set_input(&key, TRON_TEST_KEY, window, cx);
        set_input(&password, TRON_TEST_PASSWORD, window, cx);
        root.import_public_account_from_input(window, cx);
    };
    let address = root.update_in(cx, |root, window, cx| {
        root.reload_public_accounts(window, cx);
        root.select_tron(window, cx);
        let store = root.vault_store.clone().unwrap();
        let session = root.view_session.clone().unwrap();
        let evm_before = store
            .list_public_accounts_for_session(&session, true)
            .unwrap();

        import(root, window, cx);
        assert_eq!(root.public_form.error, None);
        let [account] = root.tron_public.accounts.as_slice() else {
            panic!("one Tron account");
        };
        assert_eq!(account.label.as_deref(), Some("Account #1"));
        let address = account.address;
        assert_eq!(
            store
                .list_public_accounts_for_session(&session, true)
                .unwrap(),
            evm_before
        );

        // The same key again is refused with the existing account's label.
        import(root, window, cx);
        assert_eq!(
            root.public_form.error.as_deref(),
            Some("This key is already added on Tron as Account #1.")
        );
        assert_eq!(root.tron_public.accounts.len(), 1);

        // Receive shows the label, the base58 address the QR code encodes, and a Tron note.
        let listed = root.listed_selected_account().unwrap().clone();
        let (label, shown, note) = root.public_receive_text(&listed);
        assert_eq!(label.as_deref(), Some("Account #1"));
        assert_eq!(
            shown,
            wallet_ops::tron::address::encode_tron_address(address)
        );
        assert!(note.contains("Tron address"), "{note}");
        root.open_public_address_qr_dialog(&listed, window, cx);
        assert!(gpui_component::WindowExt::has_active_dialog(window, cx));
        window.close_all_dialogs(cx);
        address
    });

    // Copying the selected account copies its base58 address.
    cx.update(|window, cx| {
        root.read(cx)
            .public_form
            .list_focus
            .clone()
            .focus(window, cx);
        window.draw(cx).clear(cx);
    });
    cx.simulate_keystrokes("c");
    cx.update(|_, cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            wallet_ops::tron::address::encode_tron_address(address)
        );
    });
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn tron_reads_start_once_tron_is_selected_and_skip_excluded_accounts(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("tron-refresh");
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
    root.update_in(cx, |root, window, cx| {
        let store = root.vault_store.clone().unwrap();
        let session = root.view_session.clone().unwrap();
        let included = store
            .add_derived_tron_account(TRON_TEST_PASSWORD, &session, None, None)
            .unwrap();
        let excluded = store
            .import_tron_account(TRON_TEST_PASSWORD, &session, TRON_TEST_KEY, None, false)
            .unwrap();
        root.reload_public_accounts(window, cx);
        root.ui_state.public_auto_refresh = true;
        root.ui_state
            .public_refresh_excluded
            .insert(excluded.tron_account_uuid);
        let route = root.tron_public.route.clone().unwrap();
        let scope = root.public_balance_scope_for_route(route).unwrap();
        let refreshing = |root: &WalletRoot| {
            root.tron_public
                .balance_cache
                .refreshing_accounts(&scope)
                .into_iter()
                .collect::<Vec<_>>()
        };

        // Until Tron is selected, neither triggers nor an explicit refresh read Tron.
        root.schedule_automatic_public_balance_refresh(cx);
        root.schedule_tron_balance_refresh([included.tron_account_uuid.clone()], cx);
        assert!(!root.tron_public.activated);
        assert_eq!(refreshing(root), [] as [std::string::String; 0]);

        // Selecting Tron reads the included account; the excluded one is skipped.
        root.select_tron(window, cx);
        assert!(root.tron_public.activated);
        assert_eq!(refreshing(root), [included.tron_account_uuid]);
    });
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}
