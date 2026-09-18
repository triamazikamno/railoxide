use std::{collections::BTreeMap, sync::Arc, time::Duration};

use broadcaster_monitor_waku::WakuMonitorConfig;
use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    TestAppContext, WeakEntity, Window, div,
};
use gpui_component::{Root, WindowExt};
use wallet_ops::{
    BroadcasterFeePolicy, DesktopWalletSyncStartPolicy, PoiReadSource, PublicTransactionTracker,
    TokenAnchorRateCache, ViewWalletChainSessionRequest, WalletSessionStore,
    settings::{
        EffectiveTokenRegistry, WalletSettings, WalletUiState, build_effective_chain_configs,
    },
    vault::{DesktopVaultStore, ExecutorStore, KdfParams, WalletSource},
};
use zeroize::Zeroizing;

use crate::root::{
    ChainUtxoState, SpendAuthorizationLifetime, VaultState, WalletAppOptions,
    WalletMaintenanceController, WalletRoot, WalletTab,
    spend_authorization::SpendAuthorizationIntent, startup::render_wallet_overlay_layers,
};

const PASSWORD: &str = "public registration test password";
const WALLET_ID: &str = "public-registration-wallet";
const MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

struct WalletTestWindow(Entity<WalletRoot>);

impl Render for WalletTestWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(self.0.clone())
            .children(render_wallet_overlay_layers(window, cx))
    }
}

#[gpui::test]
fn account_copy_controls_and_add_to_public_authorization(cx: &mut TestAppContext) {
    let path = std::env::temp_dir().join(format!(
        "railoxide-public-registration-{}",
        wallet_ops::vault::ExecutorOperationId::random()
            .unwrap()
            .opaque_id()
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    let vault = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    vault
        .create_vault_with_params(PASSWORD, KdfParams::new(1024, 1, 1))
        .unwrap();
    let metadata = vault
        .new_wallet_metadata(
            PASSWORD,
            WALLET_ID,
            0,
            WalletSource::Imported,
            "Test wallet",
        )
        .unwrap();
    vault
        .import_wallet_mnemonic_with_metadata(
            PASSWORD, WALLET_ID, 0, "english", MNEMONIC, &metadata,
        )
        .unwrap();
    let view_session = Arc::new(vault.load_view_session(PASSWORD, WALLET_ID).unwrap());
    let rpc_url = reqwest::Url::parse("http://127.0.0.1:1").unwrap();
    let poi = PoiReadSource::PoiProxy {
        rpc_url: rpc_url.clone().into(),
    };
    let http = wallet_ops::build_http_client(None).unwrap();
    let mut chain = build_effective_chain_configs(&WalletSettings::default())
        .unwrap()
        .remove(&1)
        .unwrap();
    chain.rpc_route = wallet_ops::RpcChainRoute::new(1, vec![rpc_url.clone()]);
    chain.archive_rpc_url = None;
    chain.quick_sync_enabled = false;
    chain.quick_sync_endpoint = None;
    chain.indexed_artifact_source = None;
    let sessions = WalletSessionStore::from_db(vault.db(), poi.clone()).unwrap();
    let session = Arc::new(
        runtime
            .block_on(sessions.start_view_wallet_session_immediate(
                ViewWalletChainSessionRequest {
                    view_session: view_session.clone(),
                    wallet_scope_generation: 0,
                    chain_id: 1,
                    effective_chain: Some(chain.clone()),
                    sync_start_policy: DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
                    init_block_number: Some(0),
                    sync_to_block: Some(0),
                    use_indexed_wallet_catch_up: false,
                    poi_read_source: poi.clone(),
                    rewind_wallet_cache: false,
                    progress_tx: None,
                },
                Some(rpc_url),
                &http,
            ))
            .unwrap(),
    );
    let records = ExecutorStore::new(vault.db(), view_session.clone(), 1).unwrap();
    let recipient = alloy::primitives::Address::repeat_byte(0x42);
    let delegate = chain.accepted_executor_profile().unwrap().delegate();
    let operations = [0, 1].map(|index| {
        let (_, signer) = vault
            .executor_spend_signers_for_session(
                &mut vault.create_spend_grant(PASSWORD).unwrap(),
                &view_session,
                None,
                1,
                index,
            )
            .unwrap();
        if index == 0 {
            let operation = wallet_ops::vault::ExecutorOperationId::random().unwrap();
            records
                .reserve(
                    operation,
                    delegate,
                    Some(&format!("Unshield 0.001 WETH → {recipient} (unwrap)")),
                    &[],
                )
                .unwrap();
            records.bind_address(operation, signer.address()).unwrap();
            operation
        } else {
            records
                .restore_index(index, signer.address(), delegate, &[])
                .unwrap()
                .operation()
        }
    });

    cx.update(gpui_component::init);
    cx.update(ui::theme::apply_zenburn_component_theme);
    let (host, cx) = cx.add_window_view(|window, cx| {
        let monitor_state = broadcaster_monitor::shared();
        let (events, event_rx) = broadcaster_monitor::event_channel(1);
        let cache = Arc::new(TokenAnchorRateCache::new());
        let tokens = EffectiveTokenRegistry {
            tokens: BTreeMap::new(),
        };
        // The registration flow needs no live price, relay, or RPC services.
        let refresh = wallet_ops::spawn_token_anchor_refresh_worker(
            runtime.handle(),
            cache.clone(),
            Vec::new(),
            BTreeMap::new(),
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
        let root = cx.new(|cx| {
            let mut root = WalletRoot::new(
                WalletAppOptions {
                    db_path: path.clone(),
                },
                PublicTransactionTracker::default(),
                http,
                vault.clone(),
                &[1],
                1,
                WalletUiState::default(),
                BTreeMap::from([(1, chain)]),
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
            root.vault_state = VaultState::ViewUnlocked;
            root.view_session = Some(view_session.clone());
            root.selected_wallet_id = Some(WALLET_ID.into());
            root.wallet_metadata = vec![metadata];
            root.active_wallet_tab = WalletTab::Public;
            root.focus_vault_input_on_render = false;
            let observation = session.observation_rx.borrow().clone();
            root.chain_states.insert(
                1,
                ChainUtxoState::Ready {
                    snapshot: observation.snapshot,
                    session: session.clone(),
                    observer_token: root.wallet_sync_lifecycle.prepare_startup(1).observer_token,
                    sync_tip: *session.sync_tip_rx.borrow(),
                    poi_refreshing: false,
                    ppoi_workflow_status: observation.ppoi_workflow_status,
                },
            );
            root.ensure_stealth_accounts(window, cx);
            root.stealth_accounts.as_mut().unwrap().open = true;
            root
        });
        let shell = cx.new(|_| WalletTestWindow(root));
        Root::new(shell, window, cx)
    });
    let root = host.read_with(cx, |host, cx| {
        host.view()
            .clone()
            .downcast::<WalletTestWindow>()
            .unwrap()
            .read(cx)
            .0
            .clone()
    });
    let panel = root.read_with(cx, |root, _| {
        root.stealth_accounts.as_ref().unwrap().view.clone()
    });
    cx.simulate_resize(gpui::size(gpui::px(1600.), gpui::px(900.)));
    cx.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.opened = true;
            panel.filter = super::super::view::AccountFilter::All;
            panel.refresh_visible(cx);
            cx.notify();
        });
        window.draw(cx).clear(cx);
    });
    let operation = operations[0].opaque_id();
    let address = records
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operations[0])
        .unwrap()
        .address()
        .unwrap();
    let copy = cx
        .debug_bounds(format!("stealth-row-address-{operation}").leak())
        .expect("copy control is rendered");
    cx.simulate_click(copy.center(), gpui::Modifiers::none());
    assert_eq!(
        cx.read_from_clipboard().unwrap().text(),
        Some(address.to_checksum(None))
    );
    assert!(
        panel.read_with(cx, |panel, _| panel.expanded.is_none()),
        "Copy must not expand the account"
    );
    let row = cx
        .debug_bounds(format!("stealth-row-{operation}").leak())
        .unwrap();
    cx.simulate_click(row.center(), gpui::Modifiers::none());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.expanded),
        Some(operations[0]),
        "Clicking the summary row expands its account"
    );
    assert!(cx.debug_bounds("stealth-inspector-recover").is_none());
    panel.update(cx, |panel, cx| {
        let assets = &mut panel.observations.entry(operations[0]).or_default().assets;
        assets
            .entry(wallet_ops::ExecutorAsset::Erc20(recipient))
            .or_default();
        assets
            .entry(wallet_ops::ExecutorAsset::Erc20(delegate))
            .or_default();
        cx.notify();
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let check = cx.debug_bounds("stealth-check-balances").unwrap();
    cx.simulate_click(check.center(), gpui::Modifiers::none());
    panel.read_with(cx, |panel, _| {
        assert!(panel.job.is_some());
        let assets = &panel.observations[&operations[0]].assets;
        assert_eq!(assets.len(), 3);
        assert!(
            assets.values().all(
                |balance| balance.attempt == super::super::observations::CheckAttempt::Checking
            )
        );
    });
    panel.update(cx, |panel, cx| {
        panel.stop_work(cx);
        panel.observations.clear();
        cx.notify();
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.simulate_click(copy.center(), gpui::Modifiers::none());
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.expanded),
        Some(operations[0]),
        "Copy must not collapse the account"
    );
    cx.simulate_click(row.center(), gpui::Modifiers::none());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(panel.read_with(cx, |panel, _| panel.expanded.is_none()));
    // Status pills must fit their content.
    let status = cx
        .debug_bounds(format!("stealth-outcome-{operation}").leak())
        .unwrap();
    assert!(
        status.size.width < gpui::px(130.),
        "Status badge stretched across its column: {status:?}"
    );
    // Only the mined payload is shown for a consumed nonce, identified by its transaction.
    let winner = alloy::primitives::B256::repeat_byte(32);
    let transaction = alloy::primitives::B256::repeat_byte(40);
    let superseded = alloy::primitives::B256::repeat_byte(31);
    panel.update(cx, |panel, cx| {
        use alloy::{
            eips::BlockNumHash,
            primitives::{Bytes, U256},
        };
        use wallet_ops::vault::{
            ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadContext,
            ExecutorPayloadInclusion, ExecutorPayloadPurpose, IssuedExecutorPayload,
        };
        let block = BlockNumHash::new(25_990_899, transaction);
        let payloads = [superseded, winner].map(|hash| {
            let mut payload = serde_json::to_value(IssuedExecutorPayload::new(
                U256::ZERO,
                delegate,
                hash,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(
                    Bytes::new(),
                    ExecutorNonceObservation::new(block, U256::ZERO),
                    Vec::new(),
                ),
            ))
            .unwrap();
            if hash == winner {
                payload["inclusion"] = serde_json::to_value(ExecutorPayloadInclusion::new(
                    block,
                    transaction,
                    ExecutorExecutionResult::Executed,
                ))
                .unwrap();
            }
            payload
        });
        let record = panel
            .records
            .iter_mut()
            .find(|record| record.operation() == operations[0])
            .unwrap();
        let mut saved = serde_json::to_value(&*record).unwrap();
        saved["issued"] = serde_json::json!(payloads);
        *record = serde_json::from_value(saved).unwrap();
        cx.notify();
    });
    let expand = cx
        .debug_bounds(format!("stealth-expand-{operation}").leak())
        .unwrap();
    cx.simulate_click(expand.center(), gpui::Modifiers::none());
    // The menu must stay reachable in narrow panels and with enlarged text.
    for (width, rem) in [(1600., 16.), (940., 16.), (1175., 20.)] {
        cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(900.)));
        cx.update(|window, cx| {
            gpui_component::Theme::global_mut(cx).font_size = gpui::px(rem);
            window.set_rem_size(gpui::px(rem));
            window.draw(cx).clear(cx);
        });
        let card = cx.debug_bounds("stealth-accounts-card").unwrap();
        let balances = cx.debug_bounds("stealth-balances").unwrap();
        let payloads = cx.debug_bounds("stealth-payloads").unwrap();
        assert_eq!(
            balances.left(),
            payloads.left(),
            "Inspector tables share a leading edge"
        );
        assert_eq!(
            balances.right(),
            payloads.right(),
            "Inspector tables share a trailing edge"
        );
        if width > 1500. {
            let summary = cx.debug_bounds("stealth-outcome-summary").unwrap();
            let details = cx.debug_bounds("stealth-account-details").unwrap();
            assert!(
                balances.left() >= summary.right() && payloads.left() >= details.right(),
                "Inspector tables must stay to the right of their summaries"
            );
            assert_eq!(balances.top(), summary.top());
            assert_eq!(payloads.top(), details.top());
            assert!(
                payloads.size.width < card.size.width - gpui::px(100.),
                "Wide windows must not stretch the inspector tables"
            );
        }

        let copy = cx
            .debug_bounds(format!("stealth-transaction-{winner}-{transaction}").leak())
            .unwrap();
        assert!(
            copy.right() <= card.right(),
            "Mined transaction is clipped at {width}px with {rem}px text: {copy:?}, {card:?}"
        );
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: card.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(-5000.))),
            ..Default::default()
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let copy = cx
            .debug_bounds(format!("stealth-transaction-{winner}-{transaction}").leak())
            .unwrap();
        assert!(
            copy.top() >= card.top() && copy.bottom() <= card.bottom(),
            "Transaction copy must scroll into view: {copy:?}, {card:?}"
        );
        cx.simulate_click(copy.center(), gpui::Modifiers::none());
        assert_eq!(
            cx.read_from_clipboard().unwrap().text(),
            Some(transaction.to_string())
        );
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: card.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(5000.))),
            ..Default::default()
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let check = cx.debug_bounds("stealth-check-balances").unwrap();
        assert!(
            check.right() <= card.right() && check.left() >= card.left(),
            "Account balance check is clipped at {width}px with {rem}px text: {check:?}, {card:?}"
        );
        let menu = cx
            .debug_bounds(format!("stealth-row-menu-{operation}").leak())
            .unwrap();
        assert!(
            menu.right() <= card.right(),
            "Account menu is clipped at {width}px with {rem}px text: {menu:?}, card: {card:?}"
        );
        let expand = cx
            .debug_bounds(format!("stealth-expand-{operation}").leak())
            .unwrap();
        assert!(
            expand.left() >= menu.right(),
            "The caret must follow the menu: {expand:?}, menu: {menu:?}"
        );
        assert!(
            expand.right() <= card.right(),
            "Account caret is clipped at {width}px with {rem}px text: {expand:?}, card: {card:?}"
        );
    }
    // Below the account table's minimum width, its row actions remain scrollable.
    cx.simulate_resize(gpui::size(gpui::px(800.), gpui::px(900.)));
    cx.update(|window, cx| {
        gpui_component::Theme::global_mut(cx).font_size = gpui::px(16.);
        window.set_rem_size(gpui::px(16.));
        window.draw(cx).clear(cx);
    });
    let card = cx.debug_bounds("stealth-accounts-card").unwrap();
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: card.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(-5000.), gpui::px(0.))),
        ..Default::default()
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let expand = cx
        .debug_bounds(format!("stealth-expand-{operation}").leak())
        .unwrap();
    assert!(expand.left() >= card.left() && expand.right() <= card.right());
    cx.simulate_click(expand.center(), gpui::Modifiers::none());
    assert!(panel.read_with(cx, |panel, _| panel.expanded.is_none()));
    cx.simulate_click(expand.center(), gpui::Modifiers::none());
    cx.simulate_resize(gpui::size(gpui::px(1600.), gpui::px(900.)));
    cx.update(|window, cx| {
        gpui_component::Theme::global_mut(cx).font_size = gpui::px(16.);
        window.set_rem_size(gpui::px(16.));
        window.draw(cx).clear(cx);
    });
    panel.update(cx, |panel, cx| {
        panel.reload_records();
        cx.notify();
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let expand = cx
        .debug_bounds(format!("stealth-expand-{operation}").leak())
        .unwrap();
    cx.simulate_click(expand.center(), gpui::Modifiers::none());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    // Both menu entry points share the local Hide/Unhide command.
    let other = operations[1].opaque_id();
    let menu = cx
        .debug_bounds(format!("stealth-row-menu-{other}").leak())
        .unwrap();
    cx.simulate_click(menu.center(), gpui::Modifiers::none());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.simulate_keystrokes("up enter");
    assert!(
        records
            .records()
            .unwrap()
            .iter()
            .find(|record| record.operation() == operations[1])
            .unwrap()
            .is_hidden()
    );
    cx.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.show_hidden = true;
            panel.refresh_visible(cx);
            cx.notify();
        });
        window.draw(cx).clear(cx);
    });
    let row = cx
        .debug_bounds(format!("stealth-row-{other}").leak())
        .unwrap();
    cx.simulate_event(gpui::MouseDownEvent {
        button: gpui::MouseButton::Right,
        position: row.center(),
        ..Default::default()
    });
    cx.simulate_event(gpui::MouseUpEvent {
        button: gpui::MouseButton::Right,
        position: row.center(),
        ..Default::default()
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.simulate_keystrokes("up enter");
    assert!(
        !records
            .records()
            .unwrap()
            .iter()
            .find(|record| record.operation() == operations[1])
            .unwrap()
            .is_hidden()
    );
    cx.update(|window, cx| window.draw(cx).clear(cx));
    // A retained pending transaction must remain reachable for retry even
    // without a balance observation in this session. Load its saved shape into
    // the view fixture without exposing the owner's private write API.
    panel.update(cx, |panel, cx| {
        use alloy::{eips::BlockNumHash, primitives::B256, rpc::types::TransactionRequest};
        use wallet_ops::vault::{ExecutorOperationId, ExecutorRecoveryStepKind};
        let record = panel
            .records
            .iter_mut()
            .find(|record| record.operation() == operations[1])
            .unwrap();
        let transaction = TransactionRequest {
            from: record.address(),
            chain_id: Some(1),
            nonce: Some(7),
            gas: Some(100_000),
            max_fee_per_gas: Some(2),
            max_priority_fee_per_gas: Some(1),
            ..TransactionRequest::default().to(recipient)
        };
        let mut saved = serde_json::to_value(&*record).unwrap();
        saved["recovery_transactions"] = serde_json::json!([{
            "recovery": ExecutorOperationId::random().unwrap(),
            "step": 0,
            "kind": ExecutorRecoveryStepKind::Shield,
            "transaction": transaction,
            "hash": B256::repeat_byte(4),
            "observed": BlockNumHash::new(10, B256::repeat_byte(10)),
            "remaining_gas_limit": 100_000,
            "inclusion": null,
        }]);
        *record = serde_json::from_value(saved).unwrap();
        panel.refresh_visible(cx);
        cx.notify();
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    // Right-click targets the clicked account, without checking it implicitly.
    let row = cx
        .debug_bounds(format!("stealth-row-{}", operations[1].opaque_id()).leak())
        .unwrap();
    cx.simulate_event(gpui::MouseDownEvent {
        button: gpui::MouseButton::Right,
        position: row.center(),
        ..Default::default()
    });
    cx.simulate_event(gpui::MouseUpEvent {
        button: gpui::MouseButton::Right,
        position: row.center(),
        ..Default::default()
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(panel.read_with(cx, |panel, _| panel.job.is_none()
        && panel.observations.is_empty()));
    assert!(panel.read_with(cx, |panel, _| panel.expanded.is_none()));
    cx.simulate_keystrokes("down down enter");
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.selected),
        Some(operations[1])
    );
    assert!(cx.update(WindowExt::has_active_dialog));
    cx.simulate_keystrokes("escape");
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(!cx.update(WindowExt::has_active_dialog));
    // Recovery returns focus to the row menu, which can reopen from the keyboard.
    cx.simulate_keystrokes("enter");
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.simulate_keystrokes("down down enter");
    assert!(cx.update(WindowExt::has_active_dialog));
    cx.simulate_keystrokes("escape");
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let expand = cx
        .debug_bounds(format!("stealth-expand-{operation}").leak())
        .unwrap();
    cx.simulate_click(expand.center(), gpui::Modifiers::none());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.expanded),
        Some(operations[0]),
        "The caret must toggle once, without also triggering the row"
    );
    cx.update(|window, cx| {
        panel.read(cx).recover_focus.clone().focus(window, cx);
        window.focus_next(cx);
        window.focus_next(cx);
        window.draw(cx).clear(cx);
    });
    for (key, expanded) in [("space", None), ("enter", Some(operations[0]))] {
        let keystroke = gpui::Keystroke::parse(key).unwrap();
        cx.simulate_event(gpui::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(gpui::KeyUpEvent { keystroke });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(panel.read_with(cx, |panel, _| panel.expanded), expanded);
    }
    for (id, expected) in [
        (
            format!("stealth-inspector-recipient-{operation}"),
            recipient,
        ),
        (format!("stealth-delegate-{operation}"), delegate),
    ] {
        let copy = cx
            .debug_bounds(id.leak())
            .expect("copy control is rendered");
        cx.simulate_click(copy.center(), gpui::Modifiers::none());
        assert_eq!(
            cx.read_from_clipboard().unwrap().text(),
            Some(expected.to_checksum(None))
        );
    }
    // Adding an asset is account-local and never starts a balance check.
    // The dialog owns validation, keyboard submission and focus restoration.
    {
        use gpui::Focusable as _;
        use wallet_ops::ExecutorAsset;

        let choose_custom = |cx: &mut gpui::VisualTestContext| {
            cx.simulate_keystrokes("enter");
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.simulate_input("custom");
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.simulate_keystrokes("enter");
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
        };
        let add = cx.debug_bounds("stealth-add-token").unwrap();
        cx.simulate_click(add.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.update(WindowExt::has_active_dialog));
        choose_custom(cx);
        cx.simulate_input(&recipient.to_checksum(None));
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(panel.read(cx).add_token_focus.contains_focused(window, cx));
            assert!(panel.read(cx).observations.is_empty());
        });

        let keystroke = gpui::Keystroke::parse("enter").unwrap();
        cx.simulate_event(gpui::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(gpui::KeyUpEvent { keystroke });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.update(WindowExt::has_active_dialog));
        choose_custom(cx);
        cx.simulate_keystrokes("enter");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.update(WindowExt::has_active_dialog));
        assert!(panel.read_with(cx, |panel, _| panel.error.is_some()
            && panel.observations.is_empty()));
        cx.simulate_input(&recipient.to_checksum(None));
        cx.simulate_keystrokes("enter");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(!cx.update(WindowExt::has_active_dialog));
        panel.read_with(cx, |panel, _| {
            let assets = &panel.observations[&operations[0]].assets;
            assert_eq!(assets.len(), 1);
            assert!(assets[&ExecutorAsset::Erc20(recipient)].value.is_none());
            assert!(!panel.observations.contains_key(&operations[1]));
            assert!(panel.job.is_none());
        });

        // Clicking Add token must submit even while the known-token selector
        // retains focus after a selection.
        let known = alloy::primitives::Address::repeat_byte(0x43);
        let known_key = (1, known.to_string());
        root.update(cx, |root, _| {
            root.effective_token_registry.tokens.insert(
                known_key.clone(),
                wallet_ops::settings::EffectiveTokenInfo {
                    chain_id: 1,
                    token_address: known.to_string(),
                    symbol: "Known token".into(),
                    decimals: 18,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            );
        });
        let add = cx.debug_bounds("stealth-add-token").unwrap();
        cx.simulate_click(add.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_keystrokes("enter");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_input("Known token");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.update(WindowExt::has_active_dialog));
        let submit = cx.debug_bounds("stealth-add-token-submit").unwrap();
        cx.simulate_click(submit.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.simulate_next_frame(cx);
            assert!(!window.has_active_dialog(cx));
            assert!(panel.read(cx).add_token_focus.contains_focused(window, cx));
        });
        panel.update(cx, |panel, _| {
            let assets = &mut panel.observations.get_mut(&operations[0]).unwrap().assets;
            assert!(
                assets
                    .remove(&ExecutorAsset::Erc20(known))
                    .unwrap()
                    .value
                    .is_none()
            );
            assert!(panel.job.is_none());
        });
        root.update(cx, |root, _| {
            root.effective_token_registry.tokens.remove(&known_key);
        });

        // A second account can add an NFT without changing the first account.
        panel.update(cx, |panel, cx| {
            panel.expanded = Some(operations[1]);
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let add = cx.debug_bounds("stealth-add-token").unwrap();
        cx.simulate_click(add.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let nft = cx.debug_bounds("stealth-asset-kind-erc721").unwrap();
        cx.simulate_click(nft.center(), gpui::Modifiers::none());
        cx.update(|window, cx| {
            panel
                .read(cx)
                .token_address
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
            window.draw(cx).clear(cx);
        });
        cx.simulate_input(&recipient.to_checksum(None));
        cx.update(|window, cx| {
            panel
                .read(cx)
                .token_id
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
            window.draw(cx).clear(cx);
        });
        cx.simulate_input("42");
        cx.simulate_keystrokes("enter");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(!cx.update(WindowExt::has_active_dialog));
        panel.update(cx, |panel, cx| {
            assert!(panel.observations[&operations[1]].assets.contains_key(
                &ExecutorAsset::Erc721 {
                    collection: recipient,
                    token_id: alloy::primitives::U256::from(42)
                }
            ));
            assert_eq!(panel.observations[&operations[0]].assets.len(), 1);
            assert!(panel.job.is_none());
            panel.observations.clear();
            panel.expanded = Some(operations[0]);
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
    // The summary shows only completed positive observations, independent of
    // retries. The inspector still exposes every asset and its attempt state.
    {
        use super::super::observations::{BalanceObservation, BalanceValue, CheckAttempt};
        use alloy::{eips::BlockNumHash, primitives::U256};
        use wallet_ops::ExecutorAsset;

        let token = ExecutorAsset::Erc20(recipient);
        let label = super::super::asset_label(token);
        let summary = format!("stealth-row-asset-{operation}-{label}").leak();
        let inspector = format!("stealth-asset-address-{operation}-{label}").leak();
        for (amount, attempt, visible) in [
            (Some(U256::ZERO), CheckAttempt::Available, false),
            (Some(U256::ZERO), CheckAttempt::Checking, false),
            (Some(U256::from(1)), CheckAttempt::Checking, true),
            (None, CheckAttempt::Checking, false),
            (Some(U256::ZERO), CheckAttempt::Unavailable, false),
            (Some(U256::ZERO), CheckAttempt::Stopped, false),
            (Some(U256::from(1)), CheckAttempt::Unavailable, true),
            (None, CheckAttempt::Unavailable, false),
        ] {
            cx.update(|window, cx| {
                panel.update(cx, |panel, cx| {
                    panel
                        .observations
                        .entry(operations[0])
                        .or_default()
                        .assets
                        .insert(
                            token,
                            BalanceObservation {
                                value: amount.map(|amount| BalanceValue {
                                    amount,
                                    block: BlockNumHash::default(),
                                    checked_at: std::time::SystemTime::now(),
                                }),
                                attempt,
                                attempted_at: Some(std::time::SystemTime::now()),
                            },
                        );
                    cx.notify();
                });
                window.draw(cx).clear(cx);
            });
            assert_eq!(cx.debug_bounds(summary).is_some(), visible);
            assert!(cx.debug_bounds(inspector).is_some());
            let recover = cx.debug_bounds("stealth-inspector-recover");
            assert_eq!(
                recover.is_some(),
                visible,
                "Recovery requires an observed positive balance"
            );
            if let Some(recover) = recover {
                cx.simulate_click(recover.center(), gpui::Modifiers::none());
                cx.update(|window, cx| window.draw(cx).clear(cx));
                assert!(cx.update(WindowExt::has_active_dialog));
                assert!(!cx.update(WindowExt::has_active_sheet));
                assert_eq!(
                    panel.read_with(cx, |panel, cx| panel.recovery.amount.read(cx).value()),
                    "1"
                );
                let native = cx.debug_bounds("stealth-native-funding").unwrap();
                cx.simulate_click(native.center(), gpui::Modifiers::none());
                assert!(!panel.read_with(cx, |panel, _| panel.recovery.native_funding));
                cx.simulate_keystrokes("escape");
                cx.update(|window, cx| window.draw(cx).clear(cx));
                assert!(!cx.update(WindowExt::has_active_dialog));
            }
            assert!(panel.read_with(cx, |panel, _| panel.job.is_none()));
        }
        // Selecting another asset replaces the amount with its balance. Native
        // funding also follows balance changes while the modal remains open.
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                for (asset, amount) in [
                    (
                        ExecutorAsset::Native,
                        U256::from(2_000_000_000_000_000_000u64),
                    ),
                    (token, U256::from(3)),
                ] {
                    panel
                        .observations
                        .entry(operations[0])
                        .or_default()
                        .assets
                        .insert(
                            asset,
                            BalanceObservation {
                                value: Some(BalanceValue {
                                    amount,
                                    block: BlockNumHash::default(),
                                    checked_at: std::time::SystemTime::now(),
                                }),
                                attempt: CheckAttempt::Available,
                                attempted_at: None,
                            },
                        );
                }
                panel.open_recovery(operations[0], Some(token), window, cx);
            });
            window.draw(cx).clear(cx);
        });
        assert_eq!(
            panel.read_with(cx, |panel, cx| panel.recovery.amount.read(cx).value()),
            "3"
        );
        assert!(panel.read_with(cx, |panel, _| panel.recovery.native_funding));
        for (width, rem) in [(1600., 16.), (720., 20.)] {
            cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(900.)));
            cx.update(|window, cx| {
                window.set_rem_size(gpui::px(rem));
                window.draw(cx).clear(cx);
            });
            let form = cx.debug_bounds("stealth-recovery-form").unwrap();
            assert!(form.left() >= gpui::px(0.) && form.right() <= gpui::px(width));
            assert!((form.center().x - gpui::px(width / 2.)).abs() < gpui::px(24.));
            let native = cx.debug_bounds("stealth-native-funding").unwrap();
            let paid = cx.debug_bounds("stealth-paid-funding").unwrap();
            assert!((native.right() - paid.left()).abs() <= gpui::px(1.));
            cx.simulate_click(paid.center(), gpui::Modifiers::none());
            assert!(!panel.read_with(cx, |panel, _| panel.recovery.native_funding));
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let native = cx.debug_bounds("stealth-native-funding").unwrap();
            cx.simulate_click(native.center(), gpui::Modifiers::none());
            assert!(panel.read_with(cx, |panel, _| panel.recovery.native_funding));
        }
        cx.simulate_resize(gpui::size(gpui::px(1600.), gpui::px(900.)));
        cx.update(|window, cx| {
            window.set_rem_size(gpui::px(16.));
            window.draw(cx).clear(cx);
        });
        let selector = cx.debug_bounds("stealth-recovery-select").unwrap();
        cx.simulate_click(selector.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("up enter");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.recovery.asset),
            Some(ExecutorAsset::Native)
        );
        assert_eq!(
            panel.read_with(cx, |panel, cx| panel.recovery.amount.read(cx).value()),
            "2"
        );
        panel.update(cx, |panel, cx| {
            panel
                .observations
                .get_mut(&operations[0])
                .unwrap()
                .assets
                .get_mut(&ExecutorAsset::Native)
                .unwrap()
                .value
                .as_mut()
                .unwrap()
                .amount = U256::ZERO;
            cx.notify();
        });
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(!panel.read_with(cx, |panel, _| panel.recovery.native_funding));
        // Offers arriving after the form opens appear without a load action and
        // use the same searchable picker as Private send/unshield.
        let mut offer = crate::root::tests::fee_row(1, recipient, "recovery-offer");
        offer.relay_adapt_7702 = Some(delegate);
        root.update(cx, |root, cx| {
            let ChainUtxoState::Ready { snapshot, .. } = root.chain_states.get_mut(&1).unwrap()
            else {
                panic!("ready session")
            };
            *snapshot = Arc::new(wallet_ops::ListUtxosOutput {
                chain_id: 1,
                cache_key: "recovery-test".into(),
                utxo_count: 1,
                unspent_count: 1,
                spent_count: 0,
                local_pending_spent_count: 0,
                utxos: vec![crate::root::tests::unshield_utxo_output(
                    recipient,
                    100_000_000,
                    0,
                    1,
                )],
                totals: vec![wallet_ops::TokenTotal {
                    token: recipient.to_string(),
                    total: "100000000".into(),
                    poi_verified_total: "100000000".into(),
                }],
            });
            root.monitor_state.write().upsert_fee(offer.clone());
            cx.notify();
        });
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel
                .recovery_picker_context()
                .unwrap()
                .candidates
                .len()),
            1
        );
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.open_broadcaster_picker_for_target(
                    crate::root::broadcaster_picker::BroadcasterPickerTarget::Recovery(
                        panel.downgrade(),
                    ),
                    "Recovery",
                    1,
                    recipient,
                    window,
                    cx,
                );
            });
            window.draw(cx).clear(cx);
        });
        assert_eq!(
            root.read_with(cx, |root, cx| root
                .broadcaster_picker_dialog_snapshot(cx)
                .unwrap()
                .total_count),
            1
        );
        offer.fee_expiration = std::time::UNIX_EPOCH;
        root.update(cx, |root, _| {
            root.monitor_state.write().upsert_fee(offer);
        });
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(panel.read_with(cx, |panel, _| {
            panel
                .recovery_picker_context()
                .unwrap()
                .candidates
                .is_empty()
        }));
        assert_eq!(
            root.read_with(cx, |root, cx| root
                .broadcaster_picker_dialog_snapshot(cx)
                .unwrap()
                .total_count),
            0
        );
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            cx.update(WindowExt::has_active_dialog),
            "closing the picker retains recovery"
        );
        // Authorized preparation opens progress over the form. Dismissing it
        // must not let an asynchronous failure reopen progress or review.
        let begin_preparation =
            |cx: &mut gpui::VisualTestContext| {
                cx.update(|window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.continue_recovery(
                        crate::root::stealth_accounts::recovery::RecoveryAuthorization::Prepare {
                            approval: Arc::new(panel.owner.recovery_approval(
                                operations[0],
                                ExecutorAsset::Native,
                                U256::ONE,
                                wallet_ops::ExecutorRecoveryFunding::ExecutorNative {
                                gas_fee: wallet_ops::PublicActionGasFeeSelection::Custom {
                                    max_fee_per_gas: 1_000_000_000,
                                    max_priority_fee_per_gas: 1_000_000_000,
                                },
                                },
                            ).unwrap()),
                        },
                        wallet_ops::DesktopPrivateSpendAuthorization::VaultPassword(
                            Zeroizing::new(PASSWORD.into()),
                        ),
                        window,
                        cx,
                    );
                    });
                    window.draw(cx).clear(cx);
                });
                assert!(cx.debug_bounds("stealth-recovery-progress").is_some());
            };
        begin_preparation(cx);
        assert!(panel.read_with(cx, |panel, _| panel.job.is_some()));
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("stealth-recovery-progress").is_none());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while panel.read_with(cx, |panel, _| panel.job.is_some()) {
            assert!(
                std::time::Instant::now() < deadline,
                "preparation did not finish"
            );
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
            cx.run_until_parked();
        }
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("stealth-recovery-progress").is_none());
        assert!(panel.read_with(cx, |panel, _| panel.error.is_some()
            && panel.recovery.prepared.is_none()
            && panel.pending_authorization.is_none()));
        // Form/quote invalidation must not discard the completed progress result.
        panel.update(cx, |panel, cx| {
            panel.invalidate_recovery();
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let reopen = cx.debug_bounds("stealth-recovery-show-progress").unwrap();
        cx.simulate_click(reopen.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("stealth-recovery-progress").is_some());
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));

        // Stop invalidates the pending completion, and the stopped dialog can
        // still close and reopen despite the changed job revision.
        begin_preparation(cx);
        panel.update(
            cx,
            crate::root::stealth_accounts::StealthAccountsView::stop_work,
        );
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
        cx.run_until_parked();
        assert!(panel.read_with(cx, |panel, _| panel.job.is_none()
            && panel.recovery.prepared.is_none()
            && panel.error.is_none()));
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let reopen = cx.debug_bounds("stealth-recovery-show-progress").unwrap();
        cx.simulate_click(reopen.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("stealth-recovery-progress").is_some());
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("stealth-recovery-progress").is_none());
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        panel.update(cx, |panel, cx| {
            panel.observations.clear();
            cx.notify();
        });
    }
    cx.update(|window, cx| {
        panel.read(cx).breadcrumb_focus.clone().focus(window, cx);
        window.draw(cx).clear(cx);
    });
    cx.simulate_keystrokes("enter");
    assert!(!root.read_with(cx, |root, _| root.stealth_accounts.as_ref().unwrap().open));
    cx.update(|window, cx| root.update(cx, |root, cx| root.open_stealth_accounts(window, cx)));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.expanded),
        Some(operations[0]),
        "Breadcrumb navigation must preserve the account inspector"
    );
    let menu = cx
        .debug_bounds(format!("stealth-row-menu-{operation}").leak())
        .unwrap();
    cx.simulate_click(menu.center(), gpui::Modifiers::none());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(panel.read_with(cx, |panel, _| panel.job.is_none()
        && panel.observations.is_empty()));
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.expanded),
        Some(operations[0]),
        "Opening the row menu must not collapse the account"
    );
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.simulate_input(PASSWORD);
    cx.simulate_keystrokes("enter");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        if root.read_with(cx, |root, _| {
            root.selected_public_account().is_some_and(|account| {
                records.records().unwrap()[0].public_account_uuid()
                    == Some(account.public_account_uuid.as_str())
            })
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "registration did not select its Public account"
        );
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
    }
    assert!(!root.read_with(cx, |root, _| root.stealth_accounts.as_ref().unwrap().open));
    assert!(panel.read_with(cx, |panel, _| panel.job.is_none() && panel.error.is_none()));

    // A chain switch in the same update must invalidate the queued authorization.
    cx.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_public_account(operations[1], window, cx);
        });
    });
    let command = panel.read_with(cx, |panel, _| panel.pending_authorization.clone().unwrap());
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.finish_spend_authorization(
                SpendAuthorizationIntent::StealthAccounts(panel.clone(), command),
                Zeroizing::new(PASSWORD.into()),
                SpendAuthorizationLifetime::Once,
                window,
                cx,
            );
            root.selected_chain = 137;
        });
    });
    cx.run_until_parked();
    assert!(panel.read_with(cx, |panel, _| panel.job.is_none()));
    assert!(
        records.records().unwrap()[1]
            .public_account_uuid()
            .is_none()
    );

    cx.update(|window, cx| {
        root.update(cx, WalletRoot::clear_stealth_accounts);
        window.remove_window();
    });
    runtime.block_on(async {
        session.stop().await.unwrap();
        sessions.shutdown().await;
    });
    drop((
        host,
        root,
        panel,
        records,
        session,
        sessions,
        view_session,
        vault,
    ));
    cx.run_until_parked();
    drop(entered);
    runtime.shutdown_timeout(Duration::from_secs(1));
    std::fs::remove_dir_all(path).unwrap();
}
