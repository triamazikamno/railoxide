use super::*;
use crate::root::actions::*;
use gpui::{
    AppContext, Context, Entity, Focusable, IntoElement, ParentElement, Render, Styled,
    TestAppContext, WeakEntity, Window, div,
};
use gpui_component::Root;
use wallet_ops::settings::{WalletSettings, build_effective_chain_configs};

struct PublicListWindow(Entity<WalletRoot>);
impl Render for PublicListWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .p_3()
            .pl(gpui::px(if window.viewport_size().width < gpui::px(900.) {
                60.
            } else {
                232.
            }))
            .child(
                self.0
                    .read(cx)
                    .render_public_wallet_body(&self.0, window, cx),
            )
    }
}

pub(in crate::root) fn fixture_root(
    path: &std::path::Path,
    runtime: &tokio::runtime::Runtime,
    window: &mut Window,
    cx: &mut gpui::App,
) -> Entity<WalletRoot> {
    const PASSWORD: &str = "public list test password";
    let vault = Arc::new(DesktopVaultStore::open(path.to_path_buf()).unwrap());
    vault
        .create_vault_with_params(PASSWORD, wallet_ops::vault::KdfParams::new(1024, 1, 1))
        .unwrap();
    let metadata = vault
        .new_wallet_metadata(
            PASSWORD,
            "preview",
            0,
            WalletSource::Imported,
            "Public list test",
        )
        .unwrap();
    vault.import_wallet_mnemonic_with_metadata(PASSWORD, "preview", 0, "english", "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about", &metadata).unwrap();
    let view_session = Arc::new(vault.load_view_session(PASSWORD, "preview").unwrap());
    let http = wallet_ops::build_http_client(None).unwrap();
    let monitor_state = broadcaster_monitor::shared();
    let (events, event_rx) = broadcaster_monitor::event_channel(1);
    let cache = Arc::new(TokenAnchorRateCache::new());
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256), 18);
    let tokens = EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let refresh = wallet_ops::spawn_token_anchor_refresh_worker(
        runtime.handle(),
        cache.clone(),
        Vec::new(),
        std::iter::empty().collect(),
        tokens.clone(),
        http.clone(),
    );
    let monitor = cx.new(|cx| {
        broadcaster_monitor_gpui::BroadcasterMonitorPane::new(
            monitor_state.clone(),
            event_rx.clone(),
            &[1],
            1,
            Vec::new(),
            Arc::new(|_, _| None),
            window,
            cx,
        )
    });
    let logs = cx.new(|cx| ui::logs::LogsPane::new(ui::logs::LogStore::new(1), window, cx));
    let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
    let mut chain = build_effective_chain_configs(&WalletSettings::default())
        .unwrap()
        .get(1)
        .unwrap()
        .clone();
    // Exercise the dialog without contacting real RPC endpoints from this UI fixture.
    chain.rpc_route = wallet_ops::RpcChainRoute::new(1, Vec::<reqwest::Url>::new());
    let chains = std::iter::once(chain).collect();
    let poi = PoiReadSource::PoiProxy {
        rpc_url: reqwest::Url::parse("http://127.0.0.1:1").unwrap().into(),
    };
    cx.new(|cx| {
        let mut root = WalletRoot::new(
            WalletAppOptions {
                db_path: path.to_path_buf(),
            },
            wallet_ops::PublicTransactionTracker::default(),
            http,
            vault,
            &[1],
            1,
            WalletUiState::default(),
            chains,
            tokens,
            Duration::from_mins(1),
            None,
            BroadcasterFeePolicy::default(),
            Duration::from_secs(30),
            Duration::from_secs(5),
            false,
            false,
            poi,
            runtime.handle().clone(),
            monitor_state,
            WakuMonitorConfig::default(),
            events,
            cache,
            refresh,
            event_rx,
            monitor,
            logs,
            &WeakEntity::new_invalid(),
            &maintenance,
            window,
            cx,
        );
        root.focus_vault_input_on_render = false;
        root.vault_state = VaultState::ViewUnlocked;
        root.view_session = Some(view_session);
        root.wallet_metadata = vec![metadata];
        root.selected_wallet_id = Some(Arc::from("preview"));
        root.public_accounts = (0..15)
            .map(|index| PublicAccountMetadata {
                public_account_uuid: format!("account-{index}"),
                address: Address::repeat_byte(index),
                label: if index == 13 {
                    None
                } else {
                    Some(format!("Account {index:02}"))
                },
                source: PublicAccountSource::Derived,
                scope: PublicAccountScope::PrivateWallet {
                    wallet_uuid: "preview".into(),
                },
                derivation_index: Some(u32::from(index)),
                hardware_descriptor: None,
                status: if index == 14 {
                    PublicAccountStatus::Inactive
                } else {
                    PublicAccountStatus::Active
                },
                display_order: u32::from(index),
            })
            .collect();
        root.public_balance_snapshot = Some(Arc::new(PublicBalanceSnapshot {
            chain_id: 1,
            refreshed_at: SystemTime::now(),
            accounts: root
                .public_accounts
                .iter()
                .take(14)
                .map(|account| {
                    let amount = if account.display_order == 12 {
                        U256::ZERO
                    } else {
                        uint!(1_000_000_000_000_000_000_U256)
                    };
                    PublicAccountBalance {
                        account: account.clone(),
                        observed_at: None,
                        observed_block: None,
                        balances: [
                            PublicAssetId::Native,
                            PublicAssetId::Erc20(Address::repeat_byte(42)),
                            PublicAssetId::Erc20(Address::repeat_byte(43)),
                            PublicAssetId::Erc20(Address::repeat_byte(44)),
                        ]
                        .into_iter()
                        .map(|id| PublicBalanceEntry {
                            asset: PublicBalanceAsset {
                                id,
                                symbol: if id == PublicAssetId::Native {
                                    "ETH"
                                } else {
                                    "TOKEN"
                                }
                                .into(),
                                decimals: 18,
                            },
                            amount: PublicBalanceAmount::Available(amount),
                        })
                        .collect(),
                    }
                })
                .collect(),
        }));
        root.reconcile_public_account_selection();
        root
    })
}

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
        assert!(active.is_empty());
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

/// Chains 10 and 20 without Railgun, each with an unreachable RPC so a balance scope exists.
/// Started reads never complete: the fixture's runtime is not driven.
fn use_public_only_test_chains(root: &mut WalletRoot) {
    let template = root.effective_chain_configs.get(1).unwrap().clone();
    root.effective_chain_configs = [10, 20]
        .into_iter()
        .map(|chain_id| {
            let mut chain = template.clone();
            chain.chain_id = chain_id;
            chain.railgun = None;
            chain.rpc_route = wallet_ops::RpcChainRoute::new(
                chain_id,
                vec![reqwest::Url::parse("http://127.0.0.1:1").unwrap()],
            );
            chain
        })
        .collect();
    root.selected_chain = 10;
    root.public_balance_snapshot = None;
    root.publish_gateway_desktop_state();
}

fn observed_native_balance(account: &PublicAccountMetadata) -> PublicAccountBalance {
    PublicAccountBalance {
        account: account.clone(),
        observed_at: Some(std::time::Instant::now()),
        observed_block: Some(10),
        balances: vec![PublicBalanceEntry {
            asset: PublicBalanceAsset {
                id: PublicAssetId::Native,
                symbol: "ETH".into(),
                decimals: 18,
            },
            amount: PublicBalanceAmount::Available(uint!(1_000_000_000_000_000_000_U256)),
        }],
    }
}

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

struct WalletViewWindow(Entity<WalletRoot>);
impl Render for WalletViewWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(self.0.read(cx).render_wallet_view(&self.0, window, cx))
    }
}

#[gpui::test]
fn wallet_view_shortcuts_switch_tabs_and_open_selectors_outside_dialogs(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("wallet-tabs");
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
            WalletViewWindow(root)
        });
        Root::new(view, window, cx)
    });
    let root = host.read_with(cx, |host, cx| {
        host.view()
            .clone()
            .downcast::<WalletViewWindow>()
            .unwrap()
            .read(cx)
            .0
            .clone()
    });
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
        ("ctrl-tab", WalletTab::Private),
        ("ctrl-shift-tab", WalletTab::Activity),
        ("ctrl-shift-tab", WalletTab::Public),
    ] {
        cx.simulate_keystrokes(keys);
        assert_eq!(active_tab(cx), expected, "after {keys}");
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
        gpui_component::WindowExt::open_dialog(window, cx, |dialog, _, _| dialog);
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

    // Private and Activity are unavailable without a Railgun deployment.
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
    for keys in ["ctrl-tab", "ctrl-shift-tab"] {
        cx.simulate_keystrokes(keys);
        assert_eq!(active_tab(cx), WalletTab::Public, "after {keys}");
    }

    cx.update(|window, _| window.remove_window());
    drop(host);
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
    // Two stacked dialogs with entered text; the second input has focus.
    let inputs = cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.network_health.mode = wallet_ops::WalletNetworkMode::Tor;
            root.network_health.state = wallet_ops::WalletNetworkHealthState::Ready;
            cx.notify();
        });
        let inputs = ["first draft", "second draft"]
            .map(|value| cx.new(|cx| InputState::new(window, cx).default_value(value)));
        for (ix, input) in inputs.iter().enumerate() {
            let input = input.clone();
            gpui_component::WindowExt::open_dialog(window, cx, move |dialog, _, _| {
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
                root.network_status_popover_open,
                root.network_status_popover_focus
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
        assert!(root.read(cx).network_status_error.is_some(), "recovery ran");
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

    // The lifted pill must not expose the sidebar controls beneath the dialog.
    click(
        cx,
        pill.bottom_center() + gpui::point(gpui::px(0.), gpui::px(20.)),
    );
    assert!(cx.update(|_, cx| !root.read(cx).logs_open));

    toggle_from_shortcut(cx);
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.lock_vault(window, cx);
            assert!(!root.network_status_popover_open, "lock closes popover");
            root.toggle_network_status_from_shortcut(cx);
            assert!(
                !root.network_status_popover_open,
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
