use super::*;

/// A submitted setup whose broadcaster has not confirmed it. Its fee input must
/// survive a retry, stop, and restart because the signed payload can still execute.
pub(super) fn pending_setup(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
) -> wallet_ops::vault::ExecutorRecord {
    use alloy::primitives::B256;
    use wallet_ops::vault::ExecutorInputIdentity;
    let record = executors
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    executors
        .bind_address(operation, Address::repeat_byte(3))
        .unwrap();
    let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
        "tree": 4, "position": 16197, "commitment": "0x1"
    }))
    .unwrap();
    record_setup(
        executors,
        operation,
        record.delegate(),
        B256::repeat_byte(4),
        vec![input],
    )
}

pub(super) fn record_setup(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    delegate: Address,
    payload_hash: alloy::primitives::B256,
    inputs: Vec<wallet_ops::vault::ExecutorInputIdentity>,
) -> wallet_ops::vault::ExecutorRecord {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorNonceObservation, ExecutorPayloadContext, ExecutorPayloadPurpose,
        IssuedExecutorPayload,
    };
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    executors.record_account_read(operation, observed).unwrap();
    executors
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                delegate,
                payload_hash,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), observed, inputs),
            ),
        )
        .unwrap()
}

/// The broadcaster network client a retry's review carries. The tests send nothing through
/// it, so a client that starts no networking stands in.
pub(super) fn idle_waku(
    root: &Entity<WalletRoot>,
    cx: &gpui::VisualTestContext,
) -> wallet_ops::PublicBroadcasterWaku {
    root.read_with(cx, |root, _| {
        let mut config = root.waku.config.clone();
        config.network.mode = broadcaster_monitor_waku::RelayNetworkMode::Proxy;
        wallet_ops::PublicBroadcasterWaku::new(
            config.build_client().unwrap(),
            config.to_waku_config(),
            root.http.clone(),
            root.submission_scope.clone(),
        )
    })
}

pub(super) fn test_approval() -> SwapApproval {
    SwapApproval {
        bounds: wallet_ops::vault::SwapApprovedBounds {
            sell_amount: U256::from(100),
            unshield_amount: None,
            unshield_fee_bps: U256::ZERO,
            buy_amount: U256::from(99),
            private_minimum: U256::from(98),
            shield_fee_bps: U256::from(25),
            slippage_bps: 50,
            pre_hook_gas_limit: 1_000_000,
            post_hook_gas_limit: Some(1_000_000),
            hook_cost: Some(U256::ONE),
            anchors: Vec::new(),
            destination_minimum: None,
            gas_share_bps: None,
            gas_estimate: None,
            gas_allowance: None,
            gas_price_wei: None,
            valid_for_secs: None,
            destination_shield_fee_bps: None,
            delivery_allowance: None,
            destination_setup_fee: None,
            source_setup_fee: None,
        },
        price_verified: Some(false),
        price_acknowledged: true,
        delivery: wallet_ops::vault::SwapDelivery::Reshield,
        tokens: None,
        accounts: None,
    }
}

pub(super) fn with_swap_view(
    cx: &mut TestAppContext,
    test: impl FnOnce(
        &Entity<WalletRoot>,
        &Entity<PrivateSwapsView>,
        &ExecutorStore,
        ExecutorOperationId,
        &tokio::runtime::Runtime,
        &mut gpui::VisualTestContext,
    ),
) {
    with_swap_view_and_rpc(cx, None, test);
}

/// [`with_swap_view`] with the session's chain RPC at `rpc`, such as [`SwapStubs::rpc`],
/// instead of an unreachable one.
pub(super) fn with_swap_view_and_rpc(
    cx: &mut TestAppContext,
    rpc: Option<reqwest::Url>,
    test: impl FnOnce(
        &Entity<WalletRoot>,
        &Entity<PrivateSwapsView>,
        &ExecutorStore,
        ExecutorOperationId,
        &tokio::runtime::Runtime,
        &mut gpui::VisualTestContext,
    ),
) {
    with_swap_view_and_store(
        cx,
        rpc,
        |root, swaps, executors, operation, runtime, _, cx| {
            test(root, swaps, executors, operation, runtime, cx);
        },
    );
}

/// [`with_swap_view`] whose account is reserved as a new swap's is, with its first swap use
/// approved with [`test_approval`]'s terms, which the test may replace before an order.
pub(super) fn with_approved_swap_view(
    cx: &mut TestAppContext,
    test: impl FnOnce(
        &Entity<WalletRoot>,
        &Entity<PrivateSwapsView>,
        &ExecutorStore,
        ExecutorOperationId,
        &tokio::runtime::Runtime,
        &mut gpui::VisualTestContext,
    ),
) {
    with_approved_swap_view_and_rpc(cx, None, test);
}

/// [`with_approved_swap_view`] with the session's chain RPC at `rpc`.
pub(super) fn with_approved_swap_view_and_rpc(
    cx: &mut TestAppContext,
    rpc: Option<reqwest::Url>,
    test: impl FnOnce(
        &Entity<WalletRoot>,
        &Entity<PrivateSwapsView>,
        &ExecutorStore,
        ExecutorOperationId,
        &tokio::runtime::Runtime,
        &mut gpui::VisualTestContext,
    ),
) {
    swap_view_fixture(
        cx,
        rpc,
        Some(test_approval()),
        |root, swaps, executors, operation, runtime, _, cx| {
            test(root, swaps, executors, operation, runtime, cx);
        },
    );
}

pub(super) fn with_swap_view_and_store(
    cx: &mut TestAppContext,
    rpc: Option<reqwest::Url>,
    test: impl FnOnce(
        &Entity<WalletRoot>,
        &Entity<PrivateSwapsView>,
        &ExecutorStore,
        ExecutorOperationId,
        &tokio::runtime::Runtime,
        &wallet_ops::WalletSessionStore,
        &mut gpui::VisualTestContext,
    ),
) {
    swap_view_fixture(cx, rpc, None, test);
}

/// The swap view over an account reserved for a private swap. With `approval`, the
/// reservation takes the account's first swap use with it, as a new swap's does; without
/// one, the account holds no swap use.
pub(super) fn swap_view_fixture(
    cx: &mut TestAppContext,
    rpc: Option<reqwest::Url>,
    approval: Option<SwapApproval>,
    test: impl FnOnce(
        &Entity<WalletRoot>,
        &Entity<PrivateSwapsView>,
        &ExecutorStore,
        ExecutorOperationId,
        &tokio::runtime::Runtime,
        &wallet_ops::WalletSessionStore,
        &mut gpui::VisualTestContext,
    ),
) {
    // Parallel fixtures can receive the same wall-clock timestamp. Reserve the
    // directory atomically before opening its database.
    let directory = tempfile::Builder::new()
        .prefix("swap-ui-")
        .tempdir()
        .unwrap();
    let path = directory.path().to_path_buf();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    cx.update(|cx| {
        gpui_component::init(cx);
        ui::theme::apply_zenburn_component_theme(cx);
        // Dialogs slide in over a wall-clock animation, and a test build redraws a dirty
        // window between a click's mouse down and up. Mid-animation, that redraw moves the
        // dialog, so the release can miss the element the click measured. With reduced
        // motion, a dialog settles on its first frame, as gpui-component's own tests do.
        cx.set_reduce_motion(true);
    });
    let mut fixture = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let root = crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
        let view = root.read(cx).view_session.clone().unwrap();
        let vault = root.read(cx).vault_store.clone().unwrap();
        let mut chain = root
            .read(cx)
            .effective_chain_configs
            .get(1)
            .unwrap()
            .clone();
        chain.rpc_route = wallet_ops::RpcChainRoute::new(
            1,
            vec![rpc.unwrap_or_else(|| "http://127.0.0.1:1".parse::<reqwest::Url>().unwrap())],
        );
        let private = chain.railgun.as_mut().unwrap();
        private.archive_rpc_url = None;
        private.sync.quick_sync_endpoint = None;
        private.sync.indexed_artifact_source = None;
        let poi = wallet_ops::PoiReadSource::PoiProxy {
            rpc_url: "http://127.0.0.1:1".parse::<reqwest::Url>().unwrap().into(),
        };
        let store = wallet_ops::WalletSessionStore::from_db(vault.db(), poi.clone()).unwrap();
        let mut lifecycle = WalletSyncLifecycle::new();
        let registration = lifecycle.prepare_startup(1);
        let session = runtime.block_on(async {
            let http = wallet_ops::build_wallet_network_context(wallet_ops::WalletNetworkConfig {
                network_mode: Some(wallet_ops::WalletNetworkMode::Direct),
                proxy: None,
                data_dir: &path,
            })
            .await
            .unwrap();
            Box::pin(store.start_view_wallet_session_immediate(
                wallet_ops::ViewWalletChainSessionRequest {
                    view_session: view.clone(),
                    wallet_scope_generation: registration.generation,
                    chain_id: 1,
                    effective_chain: chain.clone(),
                    sync_start_policy:
                        wallet_ops::DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
                    init_block_number: Some(0),
                    sync_to_block: Some(0),
                    use_indexed_wallet_catch_up: false,
                    poi_read_source: poi,
                    rewind_wallet_cache: false,
                    progress_tx: None,
                },
                &http,
            ))
            .await
            .unwrap()
        });
        let session = Arc::new(session);
        let operation = ExecutorOperationId::random().unwrap();
        let executors = ExecutorStore::new(vault.db(), view, 1).unwrap();
        executors
            .reserve_with_swap_approval(
                operation,
                chain.accepted_executor_profile().unwrap().delegate(),
                Some("Private swap"),
                &[
                    wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(1)),
                    wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(2)),
                ],
                approval,
                None,
            )
            .unwrap();
        let observation = session.observation_rx.borrow().clone();
        root.update(cx, |root, _| {
            // The swap disclaimer would open above the first swap dialog; its own test shows it.
            root.ui_state.swap_disclaimer_acknowledged = true;
            root.chain_states.insert(
                1,
                ChainUtxoState::Ready {
                    snapshot: observation.snapshot,
                    session: session.clone(),
                    observer_token: registration.observer_token,
                    sync_tip: wallet_ops::WalletSyncTip::default(),
                    poi_refreshing: false,
                    ppoi_workflow_status: observation.ppoi_workflow_status,
                },
            );
        });
        fixture = Some((root.clone(), store, session, executors, operation));
        let view = cx.new(|_| WalletWindow(root));
        Root::new(view, window, cx)
    });
    let (root, store, session, executors, operation) = fixture.unwrap();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let swaps = root.read_with(cx, |root, _| root.private_swaps_view().unwrap());
    assert!(swaps.read_with(cx, |swaps, _| swaps.has_shown_swaps()));

    test(&root, &swaps, &executors, operation, &runtime, &store, cx);
    cx.update(|window, _| window.remove_window());
    drop(swaps);
    drop(host);
    drop(root);
    cx.run_until_parked();
    runtime.block_on(async {
        session.stop().await.unwrap();
        store.shutdown().await;
    });
    drop(session);
    drop(store);
    drop(executors);
    drop(entered);
    drop(runtime);
    directory.close().unwrap();
}
