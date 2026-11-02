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
    root.selected_network = crate::root::SelectedNetwork::Evm(10);
    root.last_evm_chain = 10;
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
fn selecting_tron_leaves_evm_views_unavailable_and_offers_no_action(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("tron-selection");
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

        // Private and Activity stay unavailable, with Tron's reason.
        assert_eq!(root.active_wallet_tab, WalletTab::Public);
        root.select_wallet_tab(WalletTab::Private, cx);
        root.select_wallet_tab(WalletTab::Activity, cx);
        assert_eq!(root.active_wallet_tab, WalletTab::Public);
        assert!(root.railgun_unavailable_reason().contains("Tron"));

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
    drop(host);
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

const TRON_TEST_PASSWORD: &str = "public list test password";
const TRON_TEST_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";

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

const TRX_SUN: u64 = 1_000_000;

fn clock_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A Tron chain for Send on which every account exists and a reference block is the clock's.
/// With `hang`, a send stops at its reference block read, before anything is signed. Its
/// solidified blocks include every broadcast transaction, or with `includes` false, pass their
/// expiration without them. While `hold` is set, the solidified head isn't read yet. With
/// `hold_next_preparation`, the next fee parameters read waits for `release_preparation`. Token
/// transfers need no energy, except to [`TRON_TOKENLESS_RECEIVER`].
struct TronSendChain {
    hang: bool,
    reached_signing: std::sync::atomic::AtomicBool,
    hold: std::sync::atomic::AtomicBool,
    hold_next_preparation: std::sync::atomic::AtomicBool,
    release_preparation: std::sync::atomic::AtomicBool,
    broadcast: Result<wallet_ops::tron::http_api::TronBroadcast, wallet_ops::RpcBrokerError>,
    includes: bool,
    broadcasts: std::sync::Mutex<Vec<alloy::primitives::B256>>,
}

impl TronSendChain {
    fn new(
        broadcast: Result<wallet_ops::tron::http_api::TronBroadcast, wallet_ops::RpcBrokerError>,
        includes: bool,
    ) -> Self {
        Self {
            hang: false,
            reached_signing: std::sync::atomic::AtomicBool::new(false),
            hold: std::sync::atomic::AtomicBool::new(false),
            hold_next_preparation: std::sync::atomic::AtomicBool::new(false),
            release_preparation: std::sync::atomic::AtomicBool::new(false),
            broadcast,
            includes,
            broadcasts: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn broadcasts(&self) -> Vec<alloy::primitives::B256> {
        self.broadcasts.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl wallet_ops::tron::send::TronSendReads for TronSendChain {
    async fn account_exists(
        &self,
        _account: alloy::primitives::Address,
    ) -> Result<bool, wallet_ops::RpcBrokerError> {
        Ok(true)
    }

    async fn account_resources(
        &self,
        _account: alloy::primitives::Address,
    ) -> Result<wallet_ops::tron::fees::AccountResources, wallet_ops::RpcBrokerError> {
        Ok(wallet_ops::tron::fees::AccountResources::default())
    }

    async fn chain_fee_parameters(
        &self,
    ) -> Result<wallet_ops::tron::fees::ChainFeeParameters, wallet_ops::RpcBrokerError> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.hold_next_preparation.swap(false, SeqCst) {
            while !self.release_preparation.load(SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        Ok(wallet_ops::tron::fees::ChainFeeParameters {
            bandwidth_price: 1_000,
            energy_price: 100,
            create_account_fee: TRX_SUN / 10,
            create_account_system_fee: TRX_SUN,
            create_account_bandwidth_rate: 1,
        })
    }

    async fn energy_estimate(
        &self,
        _sender: alloy::primitives::Address,
        _contract: alloy::primitives::Address,
        data: alloy::primitives::Bytes,
    ) -> Result<u64, wallet_ops::RpcBrokerError> {
        // `transfer(address,uint256)`: the recipient ends the first argument word.
        let recipient = data.get(16..36).map(alloy::primitives::Address::from_slice);
        Ok(if recipient == Some(TRON_TOKENLESS_RECEIVER) {
            130_285
        } else {
            0
        })
    }

    async fn latest_block(
        &self,
    ) -> Result<wallet_ops::tron::transaction::ReferenceBlock, wallet_ops::RpcBrokerError> {
        self.reached_signing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if self.hang {
            std::future::pending::<()>().await;
        }
        Ok(wallet_ops::tron::transaction::ReferenceBlock {
            number: 100,
            hash: alloy::primitives::B256::repeat_byte(1),
            timestamp_ms: clock_ms(),
        })
    }

    async fn broadcast(
        &self,
        transaction: &wallet_ops::tron::transaction::SignedTransaction,
    ) -> Result<wallet_ops::tron::http_api::TronBroadcast, wallet_ops::RpcBrokerError> {
        self.broadcasts.lock().unwrap().push(transaction.txid());
        self.broadcast.clone()
    }

    fn now_ms(&self) -> u64 {
        clock_ms()
    }
}

impl TronSendChain {
    fn solidified_block(&self, number: u64) -> wallet_ops::tron::confirmation::TronBlock {
        let now_ms = clock_ms();
        wallet_ops::tron::confirmation::TronBlock {
            number,
            hash: alloy::primitives::B256::repeat_byte(2),
            timestamp_ms: if self.includes {
                now_ms
            } else {
                now_ms + 2 * wallet_ops::tron::transaction::EXPIRATION_WINDOW_MS
            },
            transactions: if self.includes {
                self.broadcasts()
            } else {
                Vec::new()
            },
        }
    }
}

#[async_trait::async_trait]
impl wallet_ops::tron::confirmation::TronBlockReads for TronSendChain {
    async fn solidified_head(
        &self,
    ) -> Result<wallet_ops::tron::confirmation::TronBlock, wallet_ops::RpcBrokerError> {
        while self.hold.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(self.solidified_block(101))
    }

    async fn block(
        &self,
        number: u64,
    ) -> Result<wallet_ops::tron::confirmation::TronBlock, wallet_ops::RpcBrokerError> {
        Ok(self.solidified_block(number))
    }

    async fn transaction_infos(
        &self,
        _number: u64,
    ) -> Result<Vec<wallet_ops::tron::http_api::TronTransactionInfo>, wallet_ops::RpcBrokerError>
    {
        Ok(self
            .broadcasts()
            .into_iter()
            .map(|id| wallet_ops::tron::http_api::TronTransactionInfo {
                id,
                succeeded: true,
                logs: Vec::new(),
                fee_sun: 0,
            })
            .collect())
    }
}

/// A wallet with an imported Tron account holding 10 TRX, on Tron, with that account's Send
/// form open over `chain`. Tron isn't activated, so nothing reads its route. Returns the intent
/// that approves the review of a 1 TRX send.
fn open_tron_send(
    root: &Entity<WalletRoot>,
    cx: &mut gpui::VisualTestContext,
    runtime: &tokio::runtime::Runtime,
    chain: &Arc<TronSendChain>,
) -> crate::root::public_account::TronSendIntent {
    use wallet_ops::{PublicBalanceAmount, PublicBalanceAsset, PublicBalanceEntry};
    let (uuid, sender, operation) = root.update_in(cx, |root, window, cx| {
        let account = if let Some(account) = root.tron_public.accounts.first() {
            account.clone()
        } else {
            let store = root.vault_store.clone().unwrap();
            let session = root.view_session.clone().unwrap();
            let account = store
                .import_tron_account(TRON_TEST_PASSWORD, &session, TRON_TEST_KEY, None, false)
                .unwrap();
            root.reload_public_accounts(window, cx);
            account
        };
        root.selected_network = crate::root::SelectedNetwork::Tron;
        let listed = root
            .listed_account_for_uuid(Some(&account.tron_account_uuid))
            .unwrap()
            .clone();
        root.tron_public.snapshot = Some(Arc::new(wallet_ops::PublicBalanceSnapshot {
            chain_id: wallet_ops::tron::TRON_CHAIN_ID,
            refreshed_at: std::time::SystemTime::now(),
            accounts: vec![wallet_ops::PublicAccountBalance {
                account: listed,
                balances: vec![PublicBalanceEntry {
                    asset: PublicBalanceAsset {
                        id: PublicAssetId::Native,
                        symbol: "TRX".to_owned(),
                        decimals: 6,
                    },
                    amount: PublicBalanceAmount::Available(U256::from(10 * TRX_SUN)),
                }],
                observed_at: None,
                observed_block: None,
            }],
        }));
        root.tron_public.send.backend = Some(chain.clone());
        let uuid: Arc<str> = Arc::from(account.tron_account_uuid.as_str());
        root.open_tron_send_dialog(Arc::clone(&uuid), PublicAssetId::Native, window, cx);
        let operation = root
            .tron_public
            .send
            .form
            .as_ref()
            .unwrap()
            .operation
            .clone();
        (uuid, account.address, operation)
    });
    cx.run_until_parked();
    let review = runtime
        .block_on(wallet_ops::tron::send::prepare_tron_send(
            chain.as_ref(),
            &wallet_ops::tron::send::TronSendDraft {
                sender,
                receiver: alloy::primitives::Address::repeat_byte(0x22),
                asset: wallet_ops::tron::send::TronSendAsset::Trx,
                amount: wallet_ops::tron::send::TronSendAmount::Exact(U256::from(TRX_SUN)),
                trx_balance: 10 * TRX_SUN,
            },
        ))
        .unwrap();
    crate::root::public_account::TronSendIntent {
        account_uuid: uuid,
        account_label: "Tron".to_owned(),
        source: wallet_ops::vault::TronAccountSource::Imported,
        symbol: "TRX".to_owned(),
        decimals: 6,
        icon: None,
        review,
        operation,
    }
}

fn approve_tron_send(
    root: &Entity<WalletRoot>,
    cx: &mut gpui::VisualTestContext,
    intent: crate::root::public_account::TronSendIntent,
) {
    root.update_in(cx, |root, window, cx| {
        root.submit_tron_send_authorized(
            intent,
            wallet_ops::DesktopPrivateSpendAuthorization::VaultPassword(zeroize::Zeroizing::new(
                TRON_TEST_PASSWORD.to_owned(),
            )),
            window,
            cx,
        );
        assert!(root.public_form.sending);
    });
}

fn drive_until(
    cx: &mut gpui::VisualTestContext,
    runtime: &tokio::runtime::Runtime,
    mut done: impl FnMut(&mut gpui::VisualTestContext) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done(cx) {
        assert!(std::time::Instant::now() < deadline, "timed out");
        runtime.block_on(tokio::time::sleep(std::time::Duration::from_millis(10)));
        cx.run_until_parked();
    }
}

#[gpui::test]
fn stopping_a_tron_send_before_its_handoff_broadcasts_nothing_and_returns_to_the_form(
    cx: &mut TestAppContext,
) {
    use crate::root::public_action::PublicActionStepStatus;
    let path = temp_wallet_db_root("tron-send-stop");
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
    let chain = Arc::new(TronSendChain {
        hang: true,
        ..TronSendChain::new(
            Ok(wallet_ops::tron::http_api::TronBroadcast {
                accepted: true,
                code: None,
            }),
            true,
        )
    });
    let intent = open_tron_send(&root, cx, &runtime, &chain);
    approve_tron_send(&root, cx, intent);
    // Authorized and signing-bound, not yet handed off.
    drive_until(cx, &runtime, |_| {
        chain
            .reached_signing
            .load(std::sync::atomic::Ordering::SeqCst)
    });
    root.update_in(cx, |root, window, cx| {
        root.stop_public_action_progress(cx);
        assert!(!root.public_form.sending);
        assert_eq!(
            root.public_form.action_progress[0].status,
            PublicActionStepStatus::Stopped
        );
        root.close_public_action_progress_dialog(window, cx);
        assert_eq!(root.public_form.action_progress.len(), 0);
        assert!(
            root.tron_public.send.form.is_some(),
            "the form stays open for editing"
        );
    });
    runtime.block_on(tokio::time::sleep(std::time::Duration::from_millis(50)));
    cx.run_until_parked();
    assert_eq!(chain.broadcasts(), Vec::<alloy::primitives::B256>::new());

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn a_tron_send_the_endpoint_failed_to_take_ends_only_from_solidified_blocks(
    cx: &mut TestAppContext,
) {
    use crate::root::public_action::PublicActionStepStatus;
    let path = temp_wallet_db_root("tron-send-outcome");
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
    let cases = [
        // No answer, then a solidified block includes it.
        (
            Err(wallet_ops::RpcBrokerError::Timeout),
            true,
            PublicActionStepStatus::Done,
            None,
        ),
        // A refusal, then a solidified block passes its expiration without it.
        (
            Ok(wallet_ops::tron::http_api::TronBroadcast {
                accepted: false,
                code: Some("CONTRACT_VALIDATE_ERROR".to_owned()),
            }),
            false,
            PublicActionStepStatus::Error,
            Some("The transaction expired without being included. No TRX was spent."),
        ),
    ];
    for (broadcast, includes, status, summary) in cases {
        let chain = Arc::new(TronSendChain::new(broadcast, includes));
        let intent = open_tron_send(&root, cx, &runtime, &chain);
        approve_tron_send(&root, cx, intent);
        drive_until(cx, &runtime, |cx| {
            !root.read_with(cx, |root, _| root.public_form.sending)
        });
        root.read_with(cx, |root, _| {
            let step = &root.public_form.action_progress[0];
            assert_eq!(step.status, status, "{includes}");
            assert!(step.tx_hash.is_some(), "{includes}");
            assert_eq!(
                root.public_form.action_error_summary.as_deref(),
                summary,
                "{includes}"
            );
            assert_eq!(
                root.tron_public.send.completed.is_some(),
                includes,
                "{includes}"
            );
        });
        assert_eq!(chain.broadcasts().len(), 1, "{includes}");
    }

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn selecting_an_evm_chain_stops_a_tron_send_only_before_its_handoff(cx: &mut TestAppContext) {
    use crate::root::public_action::PublicActionStepStatus;
    use std::sync::atomic::Ordering::SeqCst;
    let path = temp_wallet_db_root("tron-send-chain-switch");
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
    root.update_in(cx, |root, _, _| use_public_only_test_chains(root));
    let accepted = || {
        Ok(wallet_ops::tron::http_api::TronBroadcast {
            accepted: true,
            code: None,
        })
    };
    let sending =
        |cx: &mut gpui::VisualTestContext| root.read_with(cx, |root, _| root.public_form.sending);

    // Before its handoff, the switch stops the send and nothing is broadcast.
    let hung = Arc::new(TronSendChain {
        hang: true,
        ..TronSendChain::new(accepted(), true)
    });
    let intent = open_tron_send(&root, cx, &runtime, &hung);
    approve_tron_send(&root, cx, intent);
    drive_until(cx, &runtime, |_| hung.reached_signing.load(SeqCst));
    root.update_in(cx, |root, window, cx| {
        assert!(root.public_form.action_stop_available);
        root.select_chain(20, window, cx);
        assert_eq!(root.public_form.action_progress.len(), 0);
    });
    drive_until(cx, &runtime, |cx| !sending(cx));
    assert_eq!(hung.broadcasts(), Vec::<alloy::primitives::B256>::new());

    // After it, the send runs to its outcome under another EVM chain than it started on.
    let held = Arc::new(TronSendChain::new(accepted(), true));
    held.hold.store(true, SeqCst);
    let intent = open_tron_send(&root, cx, &runtime, &held);
    let uuid = intent.account_uuid.to_string();
    let scope = root.update_in(cx, |root, _, _| {
        root.activate_tron();
        let route = root.tron_public.route.clone().unwrap();
        root.public_balance_scope_for_route(route).unwrap()
    });
    approve_tron_send(&root, cx, intent);
    drive_until(cx, &runtime, |cx| {
        held.broadcasts().len() == 1
            && !root.read_with(cx, |root, _| root.public_form.action_stop_available)
    });
    root.update_in(cx, |root, window, cx| {
        // As if the handoff's event hadn't arrived yet: Stop and the switch still keep the send.
        root.public_form.action_stop_available = true;
        root.stop_public_action_progress(cx);
        assert!(root.public_form.sending);
        assert!(!root.public_form.action_stopped);
        root.select_chain(10, window, cx);
        assert!(root.public_form.sending);
        assert_eq!(
            root.public_form
                .action_progress
                .first()
                .map(|step| step.status),
            Some(PublicActionStepStatus::Pending)
        );
        assert!(
            root.tron_public
                .balance_cache
                .refreshing_accounts(&scope)
                .is_empty()
        );
        // The EVM chain's Shield can't take the slot the handed-off send still owns.
        let generation = root.public_form.action_generation;
        root.submit_public_shield_from_form(window, cx);
        assert!(root.public_form.sending);
        assert_eq!(
            root.public_form
                .action_progress
                .first()
                .map(|step| step.status),
            Some(PublicActionStepStatus::Pending)
        );
        assert_eq!(root.public_form.action_generation, generation);
    });
    held.hold.store(false, SeqCst);
    drive_until(cx, &runtime, |cx| !sending(cx));
    root.update_in(cx, |root, window, cx| {
        assert_eq!(root.selected_network, crate::root::SelectedNetwork::Evm(10));
        let step = &root.public_form.action_progress[0];
        assert_eq!(step.status, PublicActionStepStatus::Done);
        assert!(step.tx_hash.is_some());
        assert!(root.tron_public.send.completed.is_some());
        // The outcome fenced the sender and refreshes it.
        assert!(
            root.tron_public
                .balance_cache
                .refreshing_accounts(&scope)
                .contains(&uuid)
        );
        root.select_tron(window, cx);
        assert_eq!(
            root.public_form.action_progress[0].status,
            PublicActionStepStatus::Done
        );
        assert!(root.tron_send_progress_note().is_some());
    });
    assert_eq!(held.broadcasts().len(), 1);

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn a_running_shield_keeps_the_action_slot_from_a_tron_send_until_a_chain_switch_ends_it(
    cx: &mut TestAppContext,
) {
    use tokio::sync::mpsc::error::TryRecvError;
    use wallet_ops::{PublicActionProgressStep, PublicActionSessionEvent};
    let path = temp_wallet_db_root("tron-send-held-shield");
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
    root.update_in(cx, |root, _, _| use_public_only_test_chains(root));

    // A Shield is running: it owns the progress, its task and its command channel.
    let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
    let task = runtime.spawn(std::future::pending::<()>());
    let generation = root.update_in(cx, |root, _, _| {
        root.public_form.shielding = true;
        let generation = root.start_public_action_progress_with_steps(
            PublicActionMode::Shield,
            vec![
                PublicActionProgressStep::Approve,
                PublicActionProgressStep::Shield,
            ],
            "USDC".to_owned(),
            None,
            PublicAccountSource::Derived,
            Some(command_tx),
            None,
            true,
        );
        root.public_form.action_task_abort_handle = Some(task.abort_handle());
        generation
    });

    // Opening and submitting a ready Tron Send leaves the Shield alone.
    let chain = Arc::new(TronSendChain::new(
        Ok(wallet_ops::tron::http_api::TronBroadcast {
            accepted: true,
            code: None,
        }),
        true,
    ));
    let intent = open_tron_send(&root, cx, &runtime, &chain);
    root.update_in(cx, |root, window, cx| {
        root.tron_public.send.form.as_mut().unwrap().review = Some(intent.review);
        root.submit_tron_send_from_form(window, cx);
        assert_eq!(root.public_form.action_progress.len(), 2);
        assert_eq!(root.public_form.action_generation, generation);
        assert!(root.public_form.action_task_abort_handle.is_some());
        assert!(!root.public_form.sending);
        assert!(root.public_form.shielding);
    });
    runtime.block_on(tokio::time::sleep(Duration::from_millis(10)));
    cx.run_until_parked();
    assert!(!task.is_finished());
    assert!(matches!(command_rx.try_recv(), Err(TryRecvError::Empty)));

    // Switching to an EVM chain ends the Shield and rejects its late events.
    root.update_in(cx, |root, window, cx| {
        root.select_chain(10, window, cx);
        assert!(!root.public_form.shielding);
        assert_eq!(root.public_form.action_progress.len(), 0);
        assert_ne!(root.public_form.action_generation, generation);
        root.apply_public_action_session_event(
            generation,
            PublicActionSessionEvent::AttemptRejected {
                step: PublicActionProgressStep::Shield,
                message: "late".to_owned(),
            },
            cx,
        );
        assert!(root.public_form.action_action_error.is_none());
    });
    assert!(runtime.block_on(task).unwrap_err().is_cancelled());
    assert!(matches!(
        command_rx.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
    assert_eq!(chain.broadcasts(), Vec::<alloy::primitives::B256>::new());

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

const TRON_USDT: alloy::primitives::Address =
    alloy::primitives::address!("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c");
/// A receiver that doesn't hold the token, so a transfer to it needs more energy.
const TRON_TOKENLESS_RECEIVER: alloy::primitives::Address =
    alloy::primitives::Address::repeat_byte(0x33);

/// Replaces the Tron account's balances in the latest refresh with `trx_sun` and any USDT.
fn set_tron_balances(root: &mut WalletRoot, trx_sun: u64, usdt: Option<U256>) {
    use wallet_ops::{PublicBalanceAmount, PublicBalanceAsset, PublicBalanceEntry};
    let entry = |id, symbol: &str, amount| PublicBalanceEntry {
        asset: PublicBalanceAsset {
            id,
            symbol: symbol.to_owned(),
            decimals: 6,
        },
        amount: PublicBalanceAmount::Available(amount),
    };
    let mut snapshot = root.tron_public.snapshot.as_deref().unwrap().clone();
    snapshot.accounts[0].balances =
        std::iter::once(entry(PublicAssetId::Native, "TRX", U256::from(trx_sun)))
            .chain(usdt.map(|amount| entry(PublicAssetId::Erc20(TRON_USDT), "USDT", amount)))
            .collect();
    root.tron_public.snapshot = Some(Arc::new(snapshot));
}

fn set_tron_test_receiver(
    root: &mut WalletRoot,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) {
    let receiver = wallet_ops::tron::address::encode_tron_address(
        alloy::primitives::Address::repeat_byte(0x22),
    );
    root.set_tron_receiver(&receiver, window, cx);
}

/// Lets the open Tron Send form's pending preparation start.
fn start_tron_send_preparation(cx: &gpui::VisualTestContext) {
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(400));
    cx.run_until_parked();
}

#[gpui::test]
fn a_late_tron_send_review_never_lands_in_another_form(cx: &mut TestAppContext) {
    use std::sync::atomic::Ordering::SeqCst;
    use wallet_ops::tron::transaction::TronAsset;
    let path = temp_wallet_db_root("tron-send-late-review");
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
    let mut intent = open_tron_send(&root, cx, &runtime, &chain);
    let uuid = Arc::clone(&intent.account_uuid);
    let review = |cx: &mut gpui::VisualTestContext| {
        root.read_with(cx, |root, _| {
            root.tron_public
                .send
                .form
                .as_ref()
                .and_then(|form| form.review)
        })
    };
    let reviews_usdt = |cx: &mut gpui::VisualTestContext| {
        matches!(
            review(cx).map(|review| review.transfer.asset),
            Some(TronAsset::Trc20 { contract, .. }) if contract == TRON_USDT
        )
    };

    // The TRX form's preparation is still reading when the user opens USDT's instead and makes
    // the same edits.
    chain.hold_next_preparation.store(true, SeqCst);
    root.update_in(cx, |root, window, cx| {
        set_tron_balances(root, 10 * TRX_SUN, Some(U256::from(5 * TRX_SUN)));
        root.tron_public
            .send
            .amount_input
            .update(cx, |input, cx| input.set_value("1", window, cx));
        set_tron_test_receiver(root, window, cx);
    });
    start_tron_send_preparation(cx);
    drive_until(cx, &runtime, |_| !chain.hold_next_preparation.load(SeqCst));
    root.update_in(cx, |root, window, cx| {
        root.close_tron_send_form(window, cx);
        root.open_tron_send_dialog(
            Arc::clone(&uuid),
            PublicAssetId::Erc20(TRON_USDT),
            window,
            cx,
        );
        root.tron_public
            .send
            .amount_input
            .update(cx, |input, cx| input.set_value("1", window, cx));
        set_tron_test_receiver(root, window, cx);
    });
    start_tron_send_preparation(cx);
    drive_until(cx, &runtime, |cx| review(cx).is_some());
    assert!(reviews_usdt(cx));
    chain.release_preparation.store(true, SeqCst);
    runtime.block_on(tokio::time::sleep(Duration::from_millis(100)));
    cx.run_until_parked();
    assert!(reviews_usdt(cx), "the TRX form's review landed in USDT's");

    // Nor does a new review of an approved TRX send whose fees rose.
    intent.review.fees.maximum -= 1;
    approve_tron_send(&root, cx, intent);
    drive_until(cx, &runtime, |cx| {
        !root.read_with(cx, |root, _| root.public_form.sending)
    });
    assert!(
        reviews_usdt(cx),
        "the TRX send's new review landed in USDT's form"
    );
    root.read_with(cx, |root, _| {
        assert_eq!(root.tron_public.send.form.as_ref().unwrap().error, None);
    });

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn a_tron_token_send_notes_a_receiver_that_does_not_hold_the_token(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("tron-send-receiver-note");
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
    let uuid = open_tron_send(&root, cx, &runtime, &chain).account_uuid;
    root.update_in(cx, |root, window, cx| {
        set_tron_balances(root, 100 * TRX_SUN, Some(U256::from(5 * TRX_SUN)));
        root.close_tron_send_form(window, cx);
        root.open_tron_send_dialog(uuid, PublicAssetId::Erc20(TRON_USDT), window, cx);
        root.tron_public
            .send
            .amount_input
            .update(cx, |input, cx| input.set_value("1", window, cx));
    });

    // The test receiver's transfer costs what the transfer back to the sender does.
    for (receiver, noted) in [
        (alloy::primitives::Address::repeat_byte(0x22), false),
        (TRON_TOKENLESS_RECEIVER, true),
    ] {
        root.update_in(cx, |root, window, cx| {
            let receiver = wallet_ops::tron::address::encode_tron_address(receiver);
            root.set_tron_receiver(&receiver, window, cx);
        });
        start_tron_send_preparation(cx);
        drive_until(cx, &runtime, |cx| {
            root.read_with(cx, |root, _| {
                root.tron_public
                    .send
                    .form
                    .as_ref()
                    .is_some_and(|form| form.review.is_some())
            })
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            cx.debug_bounds("wallet-tron-send-receiver-note").is_some(),
            noted,
            "{receiver}"
        );
    }

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn a_tron_send_reviews_the_latest_trx_balance(cx: &mut TestAppContext) {
    use wallet_ops::tron::transaction::TronAsset;
    let path = temp_wallet_db_root("tron-send-latest-balance");
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
    // The form opens with 10 TRX.
    open_tron_send(&root, cx, &runtime, &chain);
    let review = |cx: &mut gpui::VisualTestContext| {
        root.read_with(cx, |root, _| {
            root.tron_public
                .send
                .form
                .as_ref()
                .and_then(|form| form.review)
        })
    };

    // Max after a refresh lowered the balance spends the new balance. The receiver is the
    // picker's second suggestion: open it, move down, press Enter.
    root.update_in(cx, |root, window, cx| {
        use ui::recipient_picker::{RecipientPickerEvent, RecipientSuggestionDirection};
        use wallet_ops::tron::address::encode_tron_address;
        set_tron_balances(root, 4 * TRX_SUN, None);
        let tron = |byte| encode_tron_address(alloy::primitives::Address::repeat_byte(byte));
        root.non_evm_address_book = [0x33, 0x22]
            .map(|byte| wallet_ops::vault::NonEvmAddressBookEntry {
                entry_uuid: format!("entry-{byte}"),
                label: format!("Entry {byte}"),
                network: wallet_ops::bridge::NonEvmNetwork::Tron,
                address: tron(byte),
                display_order: 0,
            })
            .into();
        root.tron_receiver_picker_event(&RecipientPickerEvent::Toggle, window, cx);
        let next = RecipientPickerEvent::Move(RecipientSuggestionDirection::Next);
        root.tron_receiver_picker_event(&next, window, cx);
        root.confirm_tron_receiver_suggestion(window, cx);
        let form = root.tron_public.send.form.as_ref().unwrap();
        assert_eq!(form.receiver_value, tron(0x22));
        assert!(!form.receiver_picker.open());
        root.set_tron_send_amount_to_max(window, cx);
    });
    start_tron_send_preparation(cx);
    drive_until(cx, &runtime, |cx| review(cx).is_some());
    let max = review(cx).unwrap();
    assert_eq!(max.trx_balance, 4 * TRX_SUN);
    let TronAsset::Trx { amount_sun } = max.transfer.asset else {
        panic!("a TRX send");
    };
    assert!(amount_sun <= 4 * TRX_SUN - max.fees.maximum);

    // A review from before a refresh is prepared again instead of approved.
    root.update_in(cx, |root, window, cx| {
        set_tron_balances(root, 3 * TRX_SUN, None);
        root.submit_tron_send_from_form(window, cx);
        let form = root.tron_public.send.form.as_ref().unwrap();
        assert!(form.review.is_none() && form.preparing);
    });
    start_tron_send_preparation(cx);
    drive_until(cx, &runtime, |cx| review(cx).is_some());
    assert_eq!(review(cx).unwrap().trx_balance, 3 * TRX_SUN);

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

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
