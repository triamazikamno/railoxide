use super::*;

#[gpui::test]
fn public_list_keyboard_selection_menus_and_preferences(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("public-list");
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
    cx.update(|window, cx| {
        root.read(cx)
            .public_form
            .list_focus
            .clone()
            .focus(window, cx);
        window.draw(cx).clear(cx);
    });
    cx.update(|window, cx| {
        use gpui::AsKeystroke;
        let binding = window
            .highest_precedence_binding_for_action_in(
                &RenameSelectedAccount,
                &root.read(cx).public_form.list_focus,
            )
            .unwrap();
        let formatted = gpui_component::kbd::Kbd::format(binding.keystrokes()[0].as_keystroke());
        #[cfg(target_os = "macos")]
        assert_eq!(formatted, "⌘E");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(formatted, "Ctrl+E");
    });
    // Scrolling rows leaves search visible, including with a pending row reveal.
    cx.simulate_keystrokes("up escape");
    cx.update(|window, cx| {
        root.read(cx)
            .public_form
            .list_scroll
            .set_offset(gpui::point(gpui::px(0.), gpui::px(-500.)));
        window.draw(cx).clear(cx);
    });
    let scroll_bounds = root.read_with(cx, |root, _| root.public_form.list_scroll.bounds());
    let search_before = cx.debug_bounds("public-account-search").unwrap();
    assert!(search_before.bottom() <= scroll_bounds.top());
    cx.simulate_keystrokes("/");
    cx.update(|window, cx| {
        assert!(
            root.read(cx)
                .public_form
                .search_input
                .focus_handle(cx)
                .is_focused(window)
        );
        for _ in 0..2 {
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
        }
    });
    let search_bounds = cx.debug_bounds("public-account-search").unwrap();
    assert_eq!(search_bounds, search_before);
    cx.simulate_keystrokes("/");
    assert_eq!(
        root.read_with(cx, |root, _| root.public_form.search_query.clone())
            .as_ref(),
        "/"
    );
    cx.simulate_keystrokes("down");
    cx.update(|window, cx| assert!(root.read(cx).public_form.list_focus.is_focused(window)));
    assert!(root.read_with(cx, |root, _| {
        root.public_form.selected_account_uuid.is_some()
    }));
    for (keys, expected) in [
        ("/ backspace up", "account-0"),
        ("/ down", "account-1"),
        ("/ up", "account-0"),
    ] {
        cx.simulate_keystrokes(keys);
        cx.update(|window, cx| assert!(root.read(cx).public_form.list_focus.is_focused(window)));
        assert_eq!(
            root.read_with(cx, |root, _| root.public_form.selected_account_uuid.clone())
                .as_deref(),
            Some(expected)
        );
    }
    cx.simulate_keystrokes("right right");
    root.read_with(cx, |root, _| {
        assert_eq!(
            root.public_form.selected_account_uuid.as_deref(),
            Some("account-0")
        );
        assert_eq!(root.public_form.focused_asset_index, Some(1));
    });
    cx.simulate_keystrokes("enter");
    assert!(root.read_with(cx, |root, _| root.public_form.asset_menu.is_some()));
    cx.simulate_keystrokes("enter");
    assert!(root.read_with(cx, |root, _| root.public_form.asset_menu.is_none()));
    cx.update(|window, cx| {
        assert!(root.read(cx).public_form.list_focus.is_focused(window));
        assert!(!gpui_component::WindowExt::has_active_dialog(window, cx));
    });
    cx.simulate_keystrokes("enter");
    assert!(root.read_with(cx, |root, _| root.public_form.asset_menu.is_some()));
    cx.simulate_keystrokes("escape");
    assert!(root.read_with(cx, |root, _| root.public_form.asset_menu.is_none()));
    assert_eq!(
        root.read_with(cx, |root, _| root.public_form.selected_account_uuid.clone())
            .as_deref(),
        Some("account-0")
    );
    cx.simulate_keystrokes("down");
    root.read_with(cx, |root, _| {
        assert_eq!(
            root.public_form.selected_account_uuid.as_deref(),
            Some("account-1")
        );
        assert_eq!(root.public_form.focused_asset_index, None);
    });
    cx.simulate_keystrokes("c");
    cx.update(|_, cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            format!("{:#x}", Address::repeat_byte(1))
        );
    });
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-e"
    } else {
        "ctrl-e"
    });
    cx.update(|window, cx| assert!(gpui_component::WindowExt::has_active_dialog(window, cx)));
    assert_eq!(
        root.read_with(cx, |root, _| root.public_form.editing_account_uuid.clone())
            .as_deref(),
        Some("account-1")
    );
    cx.simulate_keystrokes("escape");
    cx.update(|window, cx| {
        root.read(cx)
            .public_form
            .list_focus
            .clone()
            .focus(window, cx);
    });
    cx.simulate_keystrokes("escape");
    assert!(root.read_with(cx, |root, _| {
        root.public_form.selected_account_uuid.is_some()
    }));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let tile = cx.debug_bounds("public-asset-account-0-Native").unwrap();
    cx.simulate_click(tile.center(), gpui::Modifiers::none());
    root.read_with(cx, |root, _| {
        assert_eq!(
            root.public_form.selected_account_uuid.as_deref(),
            Some("account-0")
        );
        assert!(root.public_form.asset_menu.is_some());
    });
    cx.simulate_keystrokes("escape down");
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let more = cx.debug_bounds("public-more-assets-account-0").unwrap();
    cx.simulate_click(more.center(), gpui::Modifiers::none());
    assert_eq!(
        root.read_with(cx, |root, _| root.public_form.selected_account_uuid.clone())
            .as_deref(),
        Some("account-0")
    );
    cx.simulate_keystrokes("right");
    for (keys, mode, next_asset_index) in [
        ("enter down down enter", PublicActionMode::Send, 1),
        ("enter down enter", PublicActionMode::Shield, 2),
    ] {
        cx.simulate_keystrokes(keys);
        assert_eq!(
            root.read_with(cx, |root, _| root.public_form.action_mode),
            mode
        );
        cx.update(|window, cx| assert!(gpui_component::WindowExt::has_active_dialog(window, cx)));
        cx.simulate_keystrokes("escape");
        cx.simulate_keystrokes("right");
        assert_eq!(
            root.read_with(cx, |root, _| root.public_form.focused_asset_index),
            Some(next_asset_index)
        );
    }
    cx.simulate_keystrokes("down");
    assert_eq!(
        root.read_with(cx, |root, _| root.public_form.selected_account_uuid.clone())
            .as_deref(),
        Some("account-1")
    );
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let hide_empty = cx.debug_bounds("public-hide-empty").unwrap();
    cx.simulate_click(hide_empty.center(), gpui::Modifiers::none());
    assert_eq!(
        root.read_with(cx, |root, _| root.public_list_accounts().0.len()),
        13
    );
    let sort = cx.debug_bounds("public-account-sort").unwrap();
    cx.simulate_click(sort.center(), gpui::Modifiers::none());
    cx.simulate_keystrokes("down down down enter");
    assert_eq!(
        root.read_with(cx, |root, _| root.ui_state.public_account_sort),
        wallet_ops::settings::PublicAccountSort::Added
    );
    cx.update(|window, cx| {
        root.read(cx)
            .public_form
            .search_input
            .focus_handle(cx)
            .focus(window, cx);
    });
    cx.simulate_input("Account 14");
    cx.run_until_parked();
    root.read_with(cx, |root, _| {
        let (active, inactive) = root.public_list_accounts();
        assert_eq!(active, [] as [PublicAccountMetadata; 0]);
        assert_eq!(inactive.len(), 1);
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("public-row-account-14").is_some());
    cx.simulate_keystrokes("down");
    assert_eq!(
        root.read_with(cx, |root, _| root.public_form.selected_account_uuid.clone())
            .as_deref(),
        Some("account-14")
    );
    let clear = cx.debug_bounds("public-clear-search").unwrap();
    cx.simulate_click(clear.center(), gpui::Modifiers::none());
    root.read_with(cx, |root, _| {
        assert!(root.public_form.search_query.is_empty());
        assert_eq!(root.public_list_accounts().0.len(), 13);
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let inactive = cx.debug_bounds("public-inactive").unwrap();
    cx.simulate_click(inactive.center(), gpui::Modifiers::none());
    cx.simulate_keystrokes("up down escape");
    assert_eq!(
        root.read_with(cx, |root, _| root.public_form.selected_account_uuid.clone())
            .as_deref(),
        Some("account-14")
    );
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("public-row-account-0").is_none());
    let selected = cx.debug_bounds("public-row-account-14").unwrap();
    cx.simulate_click(selected.center(), gpui::Modifiers::none());
    assert_eq!(
        root.read_with(cx, |root, _| root.public_form.selected_account_uuid.clone())
            .as_deref(),
        Some("account-14")
    );
    // Tab from the list reaches the Active header; Enter opens it.
    cx.simulate_keystrokes("tab enter");
    assert_eq!(
        root.read_with(cx, |root, _| root.public_form.selected_account_uuid.clone())
            .as_deref(),
        Some("account-0")
    );
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn public_account_removal_selects_next_then_previous_visible_neighbour(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("public-neighbour");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(|cx| {
        gpui_component::init(cx);
        ui::theme::apply_zenburn_component_theme(cx);
    });
    let (host, cx) = cx.add_window_view(|window, cx| {
        let root = fixture_root(&path, &runtime, window, cx);
        let view = cx.new(|_| PublicListWindow(root));
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
    root.update_in(cx, |root, window, cx| {
        let store = root.vault_store.as_ref().unwrap();
        let session = root.view_session.as_ref().unwrap();
        let first = store
            .list_public_accounts_for_session(session, true)
            .unwrap()
            .into_iter()
            .find(|account| account.status == PublicAccountStatus::Active)
            .unwrap();
        let middle = store
            .import_public_account(
                "public list test password",
                session,
                "0000000000000000000000000000000000000000000000000000000000000001",
                Some("Middle"),
                false,
            )
            .unwrap();
        let last = store
            .add_derived_public_account("public list test password", session, Some("Last"))
            .unwrap();
        root.ui_state.public_account_sort = wallet_ops::settings::PublicAccountSort::Added;
        root.public_form.selected_account_uuid = None;
        root.reload_public_accounts(window, cx);
        assert_eq!(
            root.public_form.selected_account_uuid.as_deref(),
            Some(first.public_account_uuid.as_str())
        );
        root.public_form.selected_account_uuid =
            Some(Arc::from(middle.public_account_uuid.as_str()));
        root.set_public_refresh_excluded(&middle.public_account_uuid, true, cx);
        root.delete_public_account(&middle.public_account_uuid, window, cx);
        assert_eq!(
            root.public_form.selected_account_uuid.as_deref(),
            Some(last.public_account_uuid.as_str())
        );
        let saved = wallet_ops::settings::load_wallet_ui_state(
            root.vault_store.as_ref().unwrap().db().as_ref(),
        )
        .unwrap();
        assert!(saved.public_refresh_excluded.is_empty());
        root.deactivate_public_account(&last.public_account_uuid, window, cx);
        assert_eq!(
            root.public_form.selected_account_uuid.as_deref(),
            Some(first.public_account_uuid.as_str())
        );
        assert_eq!(
            root.public_accounts
                .iter()
                .find(|account| account.public_account_uuid == last.public_account_uuid)
                .unwrap()
                .status,
            PublicAccountStatus::Inactive
        );
    });
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}
