use super::*;

#[gpui::test]
#[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
fn settings_fields_commit_first_edit_after_sync_and_number_steps(cx: &mut gpui::TestAppContext) {
    use gpui::{
        AppContext as _, IntoElement, ParentElement as _, Render, Styled as _, VisualTestContext,
        div,
    };
    use gpui_component::setting::{
        NumberFieldOptions, SettingGroup, SettingItem, SettingPage, Settings as ComponentSettings,
    };
    use std::{cell::RefCell, rc::Rc};

    /// Renders one number field followed by one string field, so the string input is the
    /// window's last tab stop and the number input the one before it.
    struct FieldsHost(Entity<WalletSettingsEditor>);

    impl Render for FieldsHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            let timeout = WalletSettingsEditor::number_field(
                "test-response-timeout",
                self.0.clone(),
                NumberFieldOptions {
                    min: 1.0,
                    max: 86_400.0,
                    step: 1.0,
                },
                None,
                |settings| settings.broadcaster.response_timeout_secs as f64,
                |settings, value| settings.broadcaster.response_timeout_secs = value as u64,
            );
            let nwaku = WalletSettingsEditor::shared_string_field(
                "test-nwaku-url",
                self.0.clone(),
                |settings| settings.waku.nwaku_url.clone().unwrap_or_default(),
                |settings, value| settings.waku.nwaku_url = Some(value),
            );
            div().size_full().child(
                ComponentSettings::new("test-settings-fields").page(
                    SettingPage::new("Fields").group(
                        SettingGroup::new()
                            .item(SettingItem::new("Timeout", timeout))
                            .item(SettingItem::new("nwaku", nwaku)),
                    ),
                ),
            )
        }
    }

    cx.update(gpui_component::init);
    let path = temp_wallet_db_root("settings-field-sync");
    let store = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let owner_slot = Rc::new(RefCell::new(None));
    let slot = owner_slot.clone();
    let handle = cx.add_window(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let editor = cx.new(|cx| {
            WalletSettingsEditor::new(
                store.clone(),
                &path,
                runtime.handle().clone(),
                WalletSettings::default(),
                maintenance,
                None,
                None,
                window,
                cx,
            )
        });
        let host = cx.new(|_| FieldsHost(editor.clone()));
        *slot.borrow_mut() = Some(editor);
        gpui_component::Root::new(host, window, cx)
    });
    let owner = owner_slot.borrow_mut().take().unwrap();
    let cx = VisualTestContext::from_window(*handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(1000.0), px(700.0)));
    let redraw = |cx: &mut VisualTestContext| {
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
    };
    redraw(cx);

    // A programmatic draft change rewrites both inputs without emitting edits.
    cx.update(|_, cx| {
        owner.update(cx, |editor, cx| {
            editor.draft.broadcaster.response_timeout_secs = 10;
            editor.draft.waku.nwaku_url = Some("http://a".to_owned());
            editor.programmatic_draft_changed(cx);
        });
    });
    redraw(cx);

    // The user's first keystroke after that sync must reach the draft.
    cx.update(gpui::Window::focus_prev);
    cx.simulate_input("b");
    redraw(cx);
    let nwaku_url = cx.update(|_, cx| owner.read(cx).draft.waku.nwaku_url.clone());
    assert_eq!(nwaku_url.as_deref(), Some("http://ab"));

    cx.update(gpui::Window::focus_prev);
    cx.simulate_input("5");
    redraw(cx);
    assert_eq!(
        cx.update(|_, cx| owner.read(cx).draft.broadcaster.response_timeout_secs),
        105
    );

    // Stepping the number input updates the draft, not only the visible text.
    cx.simulate_keystrokes("up");
    redraw(cx);
    assert_eq!(
        cx.update(|_, cx| owner.read(cx).draft.broadcaster.response_timeout_secs),
        106
    );

    // Dropping the window and entities releases the test database before removal.
    cx.update(|window, _| window.remove_window());
    drop(owner);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn tron_token_addresses_are_entered_and_shown_in_base58() {
    let tron = railgun_ui::TRON_CHAIN_ID;
    let usdt = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";
    let stored = stored_token_address(tron, usdt).unwrap();
    assert!(stored.eq_ignore_ascii_case("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c"));
    assert_eq!(display_token_address(tron, &stored), usdt);
    assert_eq!(
        stored_token_address(tron, "0xa614f803b6fd780986a42c78ec9c7f77e6ded13c"),
        Err(TRON_ADDRESS_NEEDED.to_owned())
    );
}

#[gpui::test]
fn tron_settings_refuse_submit_without_an_http_api_url(cx: &mut gpui::TestAppContext) {
    use gpui::{
        AppContext as _, IntoElement, ParentElement as _, Render, Styled as _, VisualTestContext,
        div,
    };
    use gpui_component::setting::{SettingPage, Settings as ComponentSettings};
    use std::{cell::RefCell, rc::Rc};
    use wallet_ops::settings::TronEndpointSetting;

    struct TronHost(Entity<WalletSettingsEditor>);

    impl Render for TronHost {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            let page =
                SettingPage::new("Tron").groups(self.0.read(cx).tron_settings_groups(&self.0));
            div()
                .size_full()
                .child(ComponentSettings::new("test-tron-settings").page(page))
        }
    }

    cx.update(gpui_component::init);
    let path = temp_wallet_db_root("tron-settings-submit");
    let store = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut saved = WalletSettings::default();
    let mut endpoints = saved.tron.effective_endpoints();
    endpoints.insert(
        0,
        TronEndpointSetting {
            json_rpc_url: "https://json-rpc-only.example/jsonrpc".to_owned(),
            submit: false,
            ..TronEndpointSetting::default()
        },
    );
    saved.tron.endpoints = Some(endpoints);
    saved.validate().unwrap();
    let owner_slot = Rc::new(RefCell::new(None));
    let slot = owner_slot.clone();
    let handle = cx.add_window(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let editor = cx.new(|cx| {
            WalletSettingsEditor::new(
                store.clone(),
                &path,
                runtime.handle().clone(),
                saved.clone(),
                maintenance,
                None,
                None,
                window,
                cx,
            )
        });
        let host = cx.new(|_| TronHost(editor.clone()));
        *slot.borrow_mut() = Some(editor);
        gpui_component::Root::new(host, window, cx)
    });
    let owner = owner_slot.borrow_mut().take().unwrap();
    let cx = VisualTestContext::from_window(*handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(1000.0), px(1200.0)));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let bounds = cx
        .debug_bounds("tron-endpoint-0-submit")
        .expect("the JSON-RPC-only endpoint's Submit toggle is rendered");
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    let (draft, status) = cx.update(|_, cx| {
        let editor = owner.read(cx);
        (editor.draft.tron.clone(), editor.status.clone())
    });
    assert_eq!(
        draft, saved.tron,
        "Submit stays off without an HTTP API URL"
    );
    assert!(
        status.is_none(),
        "the refusal stays in the row, not the shared page banner"
    );
    assert!(
        cx.debug_bounds("tron-endpoint-0-no-http-api").is_some(),
        "the JSON-RPC-only row states why it can't submit"
    );
    assert!(
        cx.debug_bounds("tron-endpoint-1-no-http-api").is_none(),
        "a row with an HTTP API URL states no refusal"
    );

    // Dropping the window and entities releases the test database before removal.
    cx.update(|window, _| window.remove_window());
    drop(owner);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn data_folder_size_resets_after_maintenance_and_ignores_stale_walks(
    cx: &mut gpui::TestAppContext,
) {
    use crate::root::settings::DataFolderSize;
    use gpui::{AppContext as _, IntoElement, Render, div};
    use std::{cell::RefCell, rc::Rc};

    struct EmptyHost;

    impl Render for EmptyHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div()
        }
    }

    cx.update(gpui_component::init);
    let path = temp_wallet_db_root("data-folder-size");
    let store = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let owner_slot = Rc::new(RefCell::new(None));
    let slot = owner_slot.clone();
    let handle = cx.add_window(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let editor = cx.new(|cx| {
            WalletSettingsEditor::new(
                store.clone(),
                &path,
                runtime.handle().clone(),
                WalletSettings::default(),
                maintenance.clone(),
                None,
                None,
                window,
                cx,
            )
        });
        *slot.borrow_mut() = Some((editor, maintenance));
        EmptyHost
    });
    let (editor, maintenance) = owner_slot.borrow_mut().take().unwrap();

    // A reset that finishes makes a measured size stale.
    editor.update(cx, |editor, _cx| {
        editor.data_folder_size = DataFolderSize::Ready(1);
    });
    assert!(maintenance.update(cx, |controller, cx| {
        controller.start_merkle_reset(store.as_ref(), cx)
    }));
    assert!(!maintenance.read_with(cx, |controller, _cx| controller.is_idle()));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !maintenance.read_with(cx, |controller, _cx| controller.is_idle()) {
        assert!(std::time::Instant::now() < deadline, "the reset finishes");
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
        cx.run_until_parked();
    }
    assert_eq!(
        editor.read_with(cx, |editor, _cx| editor.data_folder_size),
        DataFolderSize::Unknown
    );

    // A walk superseded by a later one cannot overwrite it.
    editor.update(cx, |editor, cx| {
        editor.data_folder_size_generation = 2;
        editor.data_folder_size = DataFolderSize::Calculating(2);
        editor.finish_data_folder_size(1, DataFolderSize::Ready(99), cx);
        assert_eq!(editor.data_folder_size, DataFolderSize::Calculating(2));
    });

    handle
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    drop(editor);
    drop(maintenance);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn pre_start_editor_saves_network_changes_that_save_otherwise_refuses(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::{AppContext as _, IntoElement, Render, div};
    use std::{cell::RefCell, rc::Rc};

    struct EmptyHost;

    impl Render for EmptyHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div()
        }
    }

    cx.update(gpui_component::init);
    let path = temp_wallet_db_root("pre-start-save");
    let store = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let owner_slot = Rc::new(RefCell::new(None));
    let slot = owner_slot.clone();
    let handle = cx.add_window(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let editor = cx.new(|cx| {
            WalletSettingsEditor::new(
                store.clone(),
                &path,
                runtime.handle().clone(),
                WalletSettings::default(),
                maintenance,
                None,
                None,
                window,
                cx,
            )
        });
        *slot.borrow_mut() = Some(editor);
        EmptyHost
    });
    let editor = owner_slot.borrow_mut().take().unwrap();
    let stored_mode = || {
        wallet_ops::settings::load_wallet_settings(store.db().as_ref())
            .unwrap()
            .network
            .mode
    };

    editor.update(cx, |editor, cx| {
        editor.set_draft_network_mode(NetworkModeSetting::Direct, cx);
        assert!(!editor.save_draft(cx), "a running wallet must Apply this");
        let params = editor.prepare_prover_cache_build(cx).unwrap();
        assert_eq!(
            params.network_mode,
            wallet_ops::WalletNetworkMode::Tor,
            "an unsaved Direct draft must not reach the prover cache download"
        );
    });
    assert_eq!(stored_mode(), NetworkModeSetting::Tor);
    editor.update(cx, |editor, cx| {
        editor.set_pre_start(cx);
        assert!(editor.save_draft(cx));
    });
    assert_eq!(stored_mode(), NetworkModeSetting::Direct);

    handle
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    drop(editor);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn poi_publisher_choice_follows_draft_and_keeps_gateways(cx: &mut gpui::TestAppContext) {
    use gpui::{AppContext as _, IntoElement, Render, div};
    use std::{cell::RefCell, rc::Rc};
    use wallet_ops::settings::PoiArtifactSettings;

    struct EmptyHost;

    impl Render for EmptyHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div()
        }
    }

    cx.update(gpui_component::init);
    let path = temp_wallet_db_root("poi-publisher-choice");
    let store = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let official = PoiArtifactSettings::official_preset();
    let custom_gateways = vec!["https://gateway.example".to_owned()];
    let mut saved = WalletSettings::default();
    saved.poi.artifact.publisher_pubkey = "11".repeat(32);
    saved.poi.artifact.gateway_urls.clone_from(&custom_gateways);
    let owner_slot = Rc::new(RefCell::new(None));
    let slot = owner_slot.clone();
    let handle = cx.add_window(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let editor = cx.new(|cx| {
            WalletSettingsEditor::new(
                store.clone(),
                &path,
                runtime.handle().clone(),
                saved.clone(),
                maintenance,
                None,
                None,
                window,
                cx,
            )
        });
        *slot.borrow_mut() = Some(editor);
        EmptyHost
    });
    let editor = owner_slot.borrow_mut().take().unwrap();

    editor.update(cx, |editor, cx| {
        // A stored publisher that differs from the official preset reads as Custom.
        assert!(editor.poi_custom_publisher_selected());

        // Official restores the publisher identity and keeps the user's gateways.
        editor.select_poi_publisher(false, cx);
        assert!(!editor.poi_custom_publisher_selected());
        let artifact = &editor.draft.poi.artifact;
        assert_eq!(artifact.publisher_pubkey, official.publisher_pubkey);
        assert_eq!(artifact.manifest_source, official.manifest_source);
        assert_eq!(artifact.gateway_urls, custom_gateways);

        // Custom stays selected while its values still equal the official preset.
        editor.select_poi_publisher(true, cx);
        assert!(editor.poi_custom_publisher_selected());

        // Discarding to official saved values clears the editor-only choice.
        editor.saved = WalletSettings::default();
        editor.discard_changes(cx);
        assert!(!editor.poi_custom_publisher_selected());
    });

    handle
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    drop(editor);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn chains_page_keywords_cover_listed_chains_and_editor_fields() {
    use crate::root::settings::chains_page_keywords;
    use railgun_ui::chain_editor::ChainField;
    use wallet_ops::settings::{
        ChainContractSettings, ChainGasSettings, CustomChainSettings, NativeCurrency,
        NativeUsdPricing,
    };

    let mut draft = WalletSettings::default();
    draft.chains.custom.insert(
        999_999,
        CustomChainSettings {
            native_usd_pricing: NativeUsdPricing::default(),
            name: "My devnet".to_owned(),
            native_currency: NativeCurrency {
                name: "Dev".to_owned(),
                symbol: "DEV".to_owned(),
                decimals: 18,
            },
            rpc_endpoints: vec!["https://rpc.example".to_owned().into()],
            explorer_urls: Vec::new(),
            enabled: true,
            contracts: ChainContractSettings::default(),
            finality_depth: None,
            gas: ChainGasSettings::default(),
        },
    );
    let keywords = chains_page_keywords(&draft);
    let has = |keyword: &str| {
        keywords
            .iter()
            .any(|candidate| candidate.as_ref() == keyword)
    };

    let arbitrum = railgun_ui::chain_name(42161).expect("Arbitrum is built in");
    assert!(has(arbitrum) && has("42161"), "built-in chain name and ID");
    assert!(
        has("My devnet") && has("999999"),
        "a custom chain that exists only in the draft"
    );
    for field in ChainField::ALL {
        assert!(has(field.label()), "chain editor field {field:?}");
    }
}

#[gpui::test]
fn chain_background_activation_is_demand_based_sticky_and_stops_on_retirement(
    cx: &mut gpui::TestAppContext,
) {
    let path = temp_wallet_db_root("retired-root-warmup");
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
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            let private = root.effective_chain_configs.get(1).unwrap().clone();
            let public = |chain_id, enabled| {
                let mut chain = private.clone();
                chain.chain_id = chain_id;
                chain.enabled = enabled;
                chain.railgun = None;
                chain.rpc_route =
                    wallet_ops::RpcChainRoute::new(chain_id, Vec::<reqwest::Url>::new());
                chain
            };
            root.effective_chain_configs = [
                private.clone(),
                public(10, true),
                public(20, true),
                public(30, false),
            ]
            .into_iter()
            .collect();
            root.selected_network = crate::root::SelectedNetwork::Evm(10);
            root.last_evm_chain = 10;
            root.view_session = None;
            root.install_chain_background();
            assert_eq!(root.active_rpc_chains, [1, 10].into_iter().collect());
            root.select_chain(20, window, cx);
            root.select_chain(10, window, cx);
            assert_eq!(root.active_rpc_chains, [1, 10, 20].into_iter().collect());
            root.install_chain_background();
            root.activate_chain_background(30);
            assert_eq!(root.active_rpc_chains, [1, 10, 20].into_iter().collect());
            assert!(root.rpc_identity_warmup.is_some());
            drop(root.begin_root_replacement_shutdown(cx));
            assert!(root.rpc_identity_warmup.is_none());
            assert!(root.active_rpc_chains.is_empty());
            // Settings Apply can defer work on the outgoing root, after retirement.
            root.apply_saved_request_settings(&WalletSettings::default(), cx);
            root.activate_chain_background(1);
            assert!(root.rpc_identity_warmup.is_none());
            assert!(root.active_rpc_chains.is_empty());
        });
    });
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    drop(entered);
    drop(runtime);
    std::fs::remove_dir_all(path).unwrap();
}
