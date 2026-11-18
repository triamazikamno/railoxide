use super::super::*;
use super::*;
use crate::root::chain_load::{ChainSyncProgress, ChainUtxoState, WalletSyncLifecycle};
use alloy::primitives::B256;
use broadcaster_core::contracts::swap_math::{SWAP_MATH_ADDRESS, SWAP_MATH_CREATION_CODE};
use gpui::{IntoElement, ParentElement, Render, Styled, TestAppContext, div};
use gpui_component::{Root, WindowExt};
use std::cell::Cell;
use std::rc::Rc;
use ui::dialog::WalletDialogExt as _;
use wallet_ops::vault::ExecutorStore;

/// Mount the root as an entity, as startup does. Calling its render helper through
/// `read` would miss attempts to read the root while GPUI holds its update lease.
struct WalletWindow(Entity<WalletRoot>);

impl Render for WalletWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

/// A submitted setup whose broadcaster has not confirmed it. Its fee input must
/// survive a retry, stop, and restart because the signed payload can still execute.
fn pending_setup(
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

fn record_setup(
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
fn idle_waku(
    root: &Entity<WalletRoot>,
    cx: &gpui::VisualTestContext,
) -> wallet_ops::PublicBroadcasterWaku {
    root.read_with(cx, |root, _| {
        let mut config = root.waku_config.clone();
        config.network.mode = broadcaster_monitor_waku::RelayNetworkMode::Proxy;
        wallet_ops::PublicBroadcasterWaku::new(
            config.build_client().unwrap(),
            config.to_waku_config(),
            root.http.clone(),
            root.submission_scope.clone(),
        )
    })
}

fn test_approval() -> SwapApproval {
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

fn with_swap_view(
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
fn with_swap_view_and_rpc(
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
fn with_approved_swap_view(
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
fn with_approved_swap_view_and_rpc(
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

fn with_swap_view_and_store(
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
fn swap_view_fixture(
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

/// An order of Address 1 for Address 2 delivered as `delivery`, placed from a set-up stealth
/// account at `Address::repeat_byte(3)`, with nothing observed yet. A Bridge order deposits its
/// 99 bought for [`BRIDGE_MINIMUM`] on the destination network, and a NEAR Intents one pays
/// [`NEAR_DEPOSIT_ADDRESS`]. Returns its UID and the account's observation.
fn placed_swap(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    delivery: wallet_ops::vault::SwapDelivery,
) -> (
    broadcaster_core::contracts::cow::OrderUid,
    wallet_ops::vault::ExecutorNonceObservation,
) {
    placed_swap_with(executors, operation, delivery, |_| {})
}

struct PlacedPrivateBridge {
    operation: ExecutorOperationId,
    destination_operation: ExecutorOperationId,
    delivery: BridgeDelivery,
    uid: broadcaster_core::contracts::cow::OrderUid,
    origin_observed: wallet_ops::vault::ExecutorNonceObservation,
    destination_observed: wallet_ops::vault::ExecutorNonceObservation,
}

/// Linked Ethereum and Polygon accounts with a private Bridge order and its destination
/// shield signed. Neither the order's delivery nor the shield's execution is observed yet.
fn placed_private_bridge(
    origin: &ExecutorStore,
    destination: &ExecutorStore,
    origin_delegate: Address,
    destination_delegate: Address,
    destination_token: Address,
) -> PlacedPrivateBridge {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorNonceObservation, ExecutorPayloadContext, ExecutorPayloadPurpose,
        IssuedExecutorPayload, SwapDestinationRecord,
    };

    let operation = ExecutorOperationId::random().unwrap();
    let destination_operation = ExecutorOperationId::random().unwrap();
    let receiver = Address::repeat_byte(0x51);
    origin
        .reserve_with_swap_approval(
            operation,
            origin_delegate,
            Some("Private swap"),
            &[],
            None,
            Some(destination_operation),
        )
        .unwrap();
    destination
        .reserve_swap_destination(
            destination_operation,
            destination_delegate,
            SwapDestinationRecord {
                origin_chain: 1,
                origin_operation: operation,
                destination_token,
                outcome: None,
            },
        )
        .unwrap();
    destination
        .bind_address(destination_operation, receiver)
        .unwrap();
    // The destination account's setup won its first nonce, and its shield is signed
    // at the next one.
    let setup_hash = B256::repeat_byte(69);
    record_setup(
        destination,
        destination_operation,
        destination_delegate,
        setup_hash,
        Vec::new(),
    );
    let destination_observed =
        ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
    confirm_setup(destination, destination_operation, destination_observed);
    destination
        .record_issued(
            destination_operation,
            IssuedExecutorPayload::new(
                U256::ONE,
                destination_delegate,
                B256::repeat_byte(70),
                ExecutorPayloadPurpose::SwapDestinationShield,
                ExecutorPayloadContext::new(
                    Bytes::from_static(b"shield"),
                    destination_observed,
                    Vec::new(),
                ),
            ),
        )
        .unwrap();
    let delivery = BridgeDelivery {
        provider: BridgeProvider::Across,
        destination_chain: 137,
        receiver,
        destination_token,
        surplus: BridgeSurplus::Reshield,
        private: Some(BridgePrivateDelivery {
            on_shield_failure: BridgeShieldFailure::KeepOnDestination,
        }),
    };
    let (uid, origin_observed) = placed_swap(origin, operation, SwapDelivery::Bridge(delivery));
    PlacedPrivateBridge {
        operation,
        destination_operation,
        delivery,
        uid,
        origin_observed,
        destination_observed,
    }
}

/// [`placed_swap`] with `edit` applied to the order's approved bounds.
fn placed_swap_with(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    delivery: wallet_ops::vault::SwapDelivery,
    edit: impl FnOnce(&mut wallet_ops::vault::SwapApprovedBounds),
) -> (
    broadcaster_core::contracts::cow::OrderUid,
    wallet_ops::vault::ExecutorNonceObservation,
) {
    let order = PlacedOrder {
        uid_hash: 0x11,
        hook_hash: 7,
        valid_to: 20,
        speeds_up: None,
    };
    placed_order(executors, operation, delivery, order, edit)
}

/// One order of [`placed_order`]: the byte of its ID's hash, the byte of its pre-hook's hash,
/// which its post-hook's follows, its expiry, and the open order it speeds up.
#[derive(Clone, Copy)]
struct PlacedOrder {
    uid_hash: u8,
    hook_hash: u8,
    valid_to: u32,
    speeds_up: Option<broadcaster_core::contracts::cow::OrderUid>,
}

/// [`placed_swap_with`] for the order `placed`. A speed-up is a second order of the account
/// its open order set up: it shares that order's proof, input and pre-hook nonce, and names
/// it as the order its pre-hook invalidates.
fn placed_order(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    delivery: wallet_ops::vault::SwapDelivery,
    placed: PlacedOrder,
    edit: impl FnOnce(&mut wallet_ops::vault::SwapApprovedBounds),
) -> (
    broadcaster_core::contracts::cow::OrderUid,
    wallet_ops::vault::ExecutorNonceObservation,
) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use broadcaster_core::contracts::cow::OrderUid;
    use wallet_ops::vault::{
        AcrossOrderTerms, BridgeOrderTerms, BridgeProvider, ExecutorInputIdentity,
        ExecutorNonceObservation, ExecutorPayloadContext, ExecutorPayloadPurpose,
        IssuedExecutorPayload, NearIntentsOrderTerms, SwapAttempt, SwapDelivery, SwapProof,
        SwapRecipient, SwapTerms,
    };
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
    let setup = if placed.speeds_up.is_some() {
        executors
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    } else {
        let setup = pending_setup(executors, operation);
        executors.record_account_read(operation, observed).unwrap();
        setup
    };
    let setup_hash = setup.issued()[0].hash();
    let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
        "tree": 4, "position": 16198, "commitment": "0x2"
    }))
    .unwrap();
    let uid = OrderUid::new(
        B256::repeat_byte(placed.uid_hash),
        Address::repeat_byte(3),
        placed.valid_to,
    );
    let hook = |nonce, hash, purpose, inputs| {
        IssuedExecutorPayload::new(
            U256::from(nonce),
            setup.delegate(),
            B256::repeat_byte(hash),
            purpose,
            ExecutorPayloadContext::new(Bytes::from_static(b"hook"), observed, inputs),
        )
    };
    let mut bounds = test_approval().bounds;
    let post_hook = hook(
        2_u64,
        placed.hook_hash + 1,
        ExecutorPayloadPurpose::SwapPostHook,
        Vec::new(),
    );
    // A Private delivery shields the output with a post-hook, and an Across one deposits it.
    let (post_hook, bridge) = match delivery {
        SwapDelivery::Reshield => (Some(post_hook), None),
        SwapDelivery::External { .. } => {
            bounds.post_hook_gas_limit = None;
            (None, None)
        }
        SwapDelivery::Bridge(delivery) => {
            bounds.destination_minimum = Some(BRIDGE_MINIMUM);
            match delivery.provider {
                BridgeProvider::Across => (
                    Some(post_hook),
                    Some(BridgeOrderTerms::Across(AcrossOrderTerms {
                        spoke_pool: Address::repeat_byte(0x55),
                        input_token: Address::repeat_byte(2),
                        output_token: delivery.destination_token,
                        input_amount: bounds.buy_amount,
                        output_amount: BRIDGE_MINIMUM,
                        quote_timestamp: 1_790_000_000,
                        fill_deadline: 1_790_007_200,
                        exclusive_relayer: Address::ZERO,
                        exclusivity_parameter: 0,
                        recipient: delivery.is_private().then_some(Address::repeat_byte(0x56)),
                        message_hash: delivery.is_private().then_some(B256::repeat_byte(0x57)),
                    })),
                ),
                BridgeProvider::NearIntents => {
                    bounds.post_hook_gas_limit = None;
                    (
                        None,
                        Some(BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
                            deposit_address: NEAR_DEPOSIT_ADDRESS,
                            min_amount_out: BRIDGE_MINIMUM,
                            amount_out: BRIDGE_MINIMUM,
                            deadline: "2026-09-30T01:00:00.000Z".into(),
                            signed_quote: "{}".into(),
                        })),
                    )
                }
            }
        }
        // 1Click converts the whole payout, which needs no post-hook.
        SwapDelivery::NonEvmBridge(_) => {
            bounds.destination_minimum = Some(BRIDGE_MINIMUM);
            bounds.post_hook_gas_limit = None;
            (
                None,
                Some(BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
                    deposit_address: NEAR_DEPOSIT_ADDRESS,
                    min_amount_out: BRIDGE_MINIMUM,
                    amount_out: BRIDGE_MINIMUM,
                    deadline: "2026-09-30T01:00:00.000Z".into(),
                    signed_quote: "{}".into(),
                })),
            )
        }
    };
    edit(&mut bounds);
    executors
        .record_swap_attempt(
            operation,
            SwapAttempt {
                use_id: wallet_ops::vault::SwapUseId::first(operation),
                terms: SwapTerms::new(
                    Address::repeat_byte(1),
                    Address::repeat_byte(2),
                    SwapRecipient::new(U256::ONE, [7; 32]),
                    setup_hash,
                ),
                proof: SwapProof::new(B256::repeat_byte(6), vec![input.clone()]),
                uid,
                submission: None,
                delivery,
                bounds,
                invalidates: placed.speeds_up,
                pre_hook: hook(
                    1_u64,
                    placed.hook_hash,
                    ExecutorPayloadPurpose::SwapPreHook,
                    vec![input],
                ),
                post_hook,
                bridge,
                speed_up: placed.speeds_up,
            },
        )
        .unwrap();
    (uid, observed)
}

/// Open the swap's detail and press Recover…, which reveals the account in Stealth accounts
/// behind the dialogs and then asks recovery to open.
fn press_swap_recover(
    swaps: &Entity<PrivateSwapsView>,
    operation: ExecutorOperationId,
    stage: SwapStage,
    cx: &mut gpui::VisualTestContext,
) {
    cx.update(|window, cx| {
        swaps.update(cx, |swaps, cx| {
            swaps.reload_records();
            assert_eq!(swaps.stage(swaps.record(operation).unwrap()), stage);
            swaps.show_detail(operation, window, cx);
        });
        window.draw(cx).clear(cx);
    });
    let recover = cx.debug_bounds("swap-progress-recover").unwrap();
    cx.simulate_click(recover.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

/// A reusable set-up stealth account, whose address is `Address::repeat_byte(3)`.
fn reusable_account(executors: &ExecutorStore, operation: ExecutorOperationId) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::B256;
    use wallet_ops::vault::ExecutorNonceObservation;
    pending_setup(executors, operation);
    confirm_setup(
        executors,
        operation,
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE),
    );
}

/// Record a confirmed read of the account's execution nonce past its setup's.
fn confirm_setup(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    observed: wallet_ops::vault::ExecutorNonceObservation,
) -> wallet_ops::vault::ExecutorRecord {
    executors.record_account_read(operation, observed).unwrap()
}

fn cold_wallet_entry(address: Address) -> wallet_ops::vault::PublicAddressBookEntry {
    wallet_ops::vault::PublicAddressBookEntry {
        entry_uuid: "cold-wallet".into(),
        label: "Cold wallet".into(),
        address,
        display_order: 0,
    }
}

fn set_receiver_text(
    swaps: &mut PrivateSwapsView,
    text: &str,
    window: &mut Window,
    cx: &mut Context<'_, PrivateSwapsView>,
) {
    let input = swaps.form.as_ref().unwrap().receiver_input.clone();
    input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    // Programmatic input changes don't emit InputEvent::Change.
    swaps.receiver_edited(window, cx);
}

/// A reusable set-up stealth account in the fixture wallet's records on Polygon, whose address
/// is `Address::repeat_byte(3)`, with the store that holds it.
fn reusable_polygon_account(
    root: &Entity<WalletRoot>,
    cx: &gpui::VisualTestContext,
) -> (ExecutorStore, ExecutorOperationId) {
    let (vault, view, delegate) = root.read_with(cx, |root, _| {
        (
            root.vault_store.clone().unwrap(),
            root.view_session.clone().unwrap(),
            root.effective_chain_configs
                .get(137)
                .unwrap()
                .accepted_executor_profile()
                .unwrap()
                .delegate(),
        )
    });
    let executors = ExecutorStore::new(vault.db(), view, 137).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    executors
        .reserve(
            operation,
            delegate,
            Some("Private swap"),
            &[wallet_ops::ExecutorAsset::Erc20(STUB_POLYGON_USDT)],
        )
        .unwrap();
    reusable_account(&executors, operation);
    (executors, operation)
}

/// A setup broadcaster's offer for `fee` of `token` on `chain_id`.
fn setup_offer(
    root: &Entity<WalletRoot>,
    chain_id: u64,
    token: Address,
    fee: U256,
    cx: &gpui::VisualTestContext,
) -> PublicBroadcasterCandidate {
    let delegate = root.read_with(cx, |root, _| {
        root.effective_chain_configs
            .get(chain_id)
            .unwrap()
            .accepted_executor_profile()
            .unwrap()
            .delegate()
    });
    PublicBroadcasterCandidate {
        chain_id,
        railgun_address: "setup-offer".into(),
        identifier: None,
        token,
        fee,
        fees_id: "setup-offer".into(),
        fee_expiration: std::time::SystemTime::now() + Duration::from_secs(60),
        reliability: 0.9,
        available_wallets: 1,
        version: "8.2.3".into(),
        relay_adapt: None,
        relay_adapt_7702: Some(delegate),
        required_poi_list_keys: Vec::new(),
        viewing_public_key: [1; 32],
        address_data: broadcaster_core::crypto::railgun::AddressData {
            master_public_key: U256::ONE,
            viewing_public_key: [1; 32],
        },
        fee_policy_status: wallet_ops::BroadcasterFeePolicyStatus::UnknownAnchor,
    }
}

/// The broadcaster a test's setup progress was sent to.
fn setup_broadcaster() -> PublicBroadcasterCandidate {
    crate::root::private_swap::setup_progress::stand_in_setup_broadcaster(1, None)
}

// The address of `setup_broadcaster`.
use crate::root::private_swap::setup_progress::STAND_IN_SETUP_BROADCASTER as SETUP_BROADCASTER;

/// The wallet's operation client, as the form opens one for each fee estimate.
fn operation_client(
    root: &Entity<WalletRoot>,
    runtime: &tokio::runtime::Runtime,
    cx: &gpui::VisualTestContext,
) -> wallet_ops::OperationHttpClient {
    let http = root.read_with(cx, |root, _| root.http.clone());
    runtime.block_on(http.operation_http_client()).unwrap()
}

/// Whether the open picker lists Polygon as a network that can be picked.
fn picker_polygon(swaps: &PrivateSwapsView, cx: &App) -> NetworkAvailability {
    let content = swaps.buy_picker_content(swaps.form.as_ref().unwrap(), cx);
    content
        .networks
        .iter()
        .find(|network| network.key == BuyNetworkKey::Chain(137))
        .unwrap()
        .availability
}

/// The destination minimum of the Bridge orders [`placed_swap`] places: 248.71 of a 6-decimal
/// token.
const BRIDGE_MINIMUM: U256 = U256::from_limbs([248_710_000, 0, 0, 0]);
/// The 1Click deposit address of the NEAR Intents orders [`placed_swap`] places.
const NEAR_DEPOSIT_ADDRESS: Address =
    alloy::primitives::address!("4444444444444444444444444444444444444444");

/// A traded Bridge order's observations at the account's `observed` block: its settlement
/// handed off to the bridge, with an Across `deposit_id` or none for NEAR Intents, and the
/// bridge reported `outcome` so far.
fn bridge_observations(
    observed: wallet_ops::vault::ExecutorNonceObservation,
    deposit_id: Option<U256>,
    outcome: Option<wallet_ops::vault::SwapBridgeOutcome>,
) -> wallet_ops::vault::SwapOrderObservations {
    use wallet_ops::vault::{
        SwapBridgeHandoff, SwapObservation, SwapOrderObservations, SwapTradeAmounts,
    };
    let seen = SwapObservation {
        block: observed.block(),
        transaction_hash: Some(alloy::primitives::B256::repeat_byte(40)),
    };
    SwapOrderObservations {
        pre_hook_executed: Some(seen),
        traded: Some(seen),
        trade_amounts: Some(SwapTradeAmounts {
            sell_amount: U256::from(100),
            buy_amount: U256::from(99),
            fee_amount: U256::ZERO,
            settlement_gas_used: None,
            settlement_effective_gas_price: None,
            executed_fee: None,
            executed_fee_token: None,
        }),
        delivered: Some(seen),
        bridge_handoff: Some(SwapBridgeHandoff {
            observation: seen,
            deposit_id,
        }),
        bridge_outcome: outcome,
        ..Default::default()
    }
}

/// Expires the Bridge order `first` untraded with `expired`, then places a retry of its swap
/// use at the same nonces, with `edit` applied to the retry's approved bounds, and records the
/// trade on the retry. Returns the retry's UID.
fn retried_and_traded(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    first: broadcaster_core::contracts::cow::OrderUid,
    observed: wallet_ops::vault::ExecutorNonceObservation,
    expired: wallet_ops::vault::SwapPreHookDeath,
    edit: impl FnOnce(&mut wallet_ops::vault::SwapApprovedBounds),
) -> broadcaster_core::contracts::cow::OrderUid {
    use alloy::primitives::{B256, Bytes};
    use broadcaster_core::contracts::cow::OrderUid;
    use wallet_ops::vault::{
        ExecutorPayloadContext, ExecutorPayloadPurpose, IssuedExecutorPayload, SwapAttempt,
        SwapOrderObservations,
    };

    executors
        .record_swap_observations(
            operation,
            first,
            SwapOrderObservations {
                pre_hook_dead: Some(expired),
                ..Default::default()
            },
        )
        .unwrap();
    let record = executors
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    let delegate = record.issued()[0].delegate();
    let swap = record.swap().unwrap();
    let order = &swap.orders()[0];
    let inputs = swap.proof().inputs().to_vec();
    let hook = |nonce: u64, hash, purpose, inputs| {
        IssuedExecutorPayload::new(
            U256::from(nonce),
            delegate,
            B256::repeat_byte(hash),
            purpose,
            ExecutorPayloadContext::new(Bytes::from_static(b"retry"), observed, inputs),
        )
    };
    let retry = OrderUid::new(B256::repeat_byte(0x12), Address::repeat_byte(3), 20);
    let mut bounds = order.bounds().clone();
    edit(&mut bounds);
    executors
        .record_swap_attempt(
            operation,
            SwapAttempt {
                use_id: order.use_id().unwrap(),
                terms: *swap.terms(),
                proof: swap.proof().clone(),
                uid: retry,
                submission: None,
                delivery: order.delivery(),
                bounds,
                invalidates: None,
                pre_hook: hook(1, 0x21, ExecutorPayloadPurpose::SwapPreHook, inputs),
                post_hook: order
                    .delivery()
                    .has_post_hook()
                    .then(|| hook(2, 0x22, ExecutorPayloadPurpose::SwapPostHook, Vec::new())),
                bridge: order.bridge().cloned(),
                speed_up: None,
            },
        )
        .unwrap();
    let traded = bridge_observations(observed, None, None);
    executors
        .record_swap_observations(
            operation,
            retry,
            SwapOrderObservations {
                traded: traded.traded,
                trade_amounts: traded.trade_amounts,
                ..Default::default()
            },
        )
        .unwrap();
    retry
}

/// `handed_off`, a traded Bridge order's observations, as the attempt that expired untraded
/// holds them once a later attempt's settlement made its deposit.
fn deposit_of_a_later_trade(
    handed_off: &wallet_ops::vault::SwapOrderObservations,
    expired: wallet_ops::vault::SwapPreHookDeath,
) -> wallet_ops::vault::SwapOrderObservations {
    wallet_ops::vault::SwapOrderObservations {
        pre_hook_executed: None,
        traded: None,
        trade_amounts: None,
        pre_hook_dead: Some(expired),
        ..handed_off.clone()
    }
}

const STUB_USDC: Address = alloy::primitives::address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
/// What the stub orderbook quotes for any sell amount: 0.0004 ETH.
const STUB_BUY_AMOUNT: u64 = 400_000_000_000_000;
/// The fee the stub orderbook reports it charged any order, in `Address::repeat_byte(2)`.
const STUB_EXECUTED_FEE: u64 = 464_572;

/// Each stub request's path and the `x-route` tag of the client that sent it.
type StubRequests = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

/// Local stand-ins for the chain RPC, the `CoW` orderbook and the bridge providers, served from
/// their own thread, so a quote goes through the real planning and review without live
/// services. The RPC answers `eth_gasPrice` and fails everything else, like the unreachable RPC
/// of other tests, with a gas price of 1 wei unless set. The orderbook quotes
/// [`STUB_BUY_AMOUNT`] for any order, with no network fee unless set, and keeps each request's
/// body, and reports every order
/// fulfilled with [`STUB_EXECUTED_FEE`], keeping each report's path. Across and 1Click list the routes of [`stub_bridge_list`] and quote as
/// [`stub_bridge_reply`] describes.
struct SwapStubs {
    url: reqwest::Url,
    quotes: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    bridge_requests: Arc<std::sync::Mutex<Vec<String>>>,
    order_requests: Arc<std::sync::Mutex<Vec<String>>>,
    /// The path of each request, which tells apart chains given their own RPC path, with the
    /// `x-route` tag of the client it came from.
    requests: StubRequests,
    across_fee_bps: Arc<std::sync::atomic::AtomicU64>,
    across_relayer_gas_bps: Arc<std::sync::atomic::AtomicU64>,
    across_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    failing: Arc<std::sync::Mutex<Vec<&'static str>>>,
    gas_price_wei: Arc<std::sync::atomic::AtomicU64>,
    public_review_rpc: Arc<std::sync::atomic::AtomicBool>,
    public_allowance_complete: Arc<std::sync::atomic::AtomicBool>,
    math_deployed: Arc<std::sync::atomic::AtomicBool>,
    fee_amount: Arc<std::sync::atomic::AtomicU64>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// What the stub providers quote with: Across deposits into the swap chain's `spoke_pool` and
/// keeps `across_fee_bps` of the amount, after `across_delay_ms`. Requests to a path starting
/// with one of `failing` get a 503.
#[derive(Clone)]
struct StubBridge {
    spoke_pool: Address,
    across_fee_bps: Arc<std::sync::atomic::AtomicU64>,
    across_relayer_gas_bps: Arc<std::sync::atomic::AtomicU64>,
    across_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    failing: Arc<std::sync::Mutex<Vec<&'static str>>>,
    public_review_rpc: Arc<std::sync::atomic::AtomicBool>,
    public_allowance_complete: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the math contract an order's post-hook calls has its code on the stub chain.
    math_deployed: Arc<std::sync::atomic::AtomicBool>,
}

impl SwapStubs {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let quotes = Arc::<std::sync::Mutex<Vec<serde_json::Value>>>::default();
        let bridge_requests = Arc::<std::sync::Mutex<Vec<String>>>::default();
        let order_requests = Arc::<std::sync::Mutex<Vec<String>>>::default();
        let requests = Arc::<std::sync::Mutex<Vec<(String, Option<String>)>>>::default();
        let across_fee_bps = Arc::new(std::sync::atomic::AtomicU64::new(10));
        let across_relayer_gas_bps = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let across_delay_ms = Arc::<std::sync::atomic::AtomicU64>::default();
        let failing = Arc::<std::sync::Mutex<Vec<&'static str>>>::default();
        let gas_price_wei = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let public_review_rpc = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let public_allowance_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let math_deployed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let served_gas_price = Arc::clone(&gas_price_wei);
        let fee_amount = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let served_fee_amount = Arc::clone(&fee_amount);
        let recorded = Arc::clone(&quotes);
        let recorded_bridge = Arc::clone(&bridge_requests);
        let recorded_orders = Arc::clone(&order_requests);
        let recorded_requests = Arc::clone(&requests);
        let bridge = StubBridge {
            spoke_pool: wallet_ops::settings::build_effective_chain_configs(
                &wallet_ops::settings::WalletSettings::default(),
            )
            .unwrap()
            .get(1)
            .unwrap()
            .bridge_profile()
            .unwrap()
            .spoke_pool(),
            across_fee_bps: Arc::clone(&across_fee_bps),
            across_relayer_gas_bps: Arc::clone(&across_relayer_gas_bps),
            across_delay_ms: Arc::clone(&across_delay_ms),
            failing: Arc::clone(&failing),
            public_review_rpc: Arc::clone(&public_review_rpc),
            public_allowance_complete: Arc::clone(&public_allowance_complete),
            math_deployed: Arc::clone(&math_deployed),
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let serve = async {
                    while let Ok((stream, _)) = listener.accept().await {
                        tokio::spawn(stub_response(
                            stream,
                            Arc::clone(&recorded),
                            Arc::clone(&recorded_bridge),
                            Arc::clone(&recorded_orders),
                            Arc::clone(&recorded_requests),
                            bridge.clone(),
                            Arc::clone(&served_gas_price),
                            Arc::clone(&served_fee_amount),
                        ));
                    }
                };
                tokio::select! {
                    () = serve => {}
                    _ = stopped => {}
                }
            });
        });
        Self {
            url,
            quotes,
            bridge_requests,
            order_requests,
            requests,
            across_fee_bps,
            across_relayer_gas_bps,
            across_delay_ms,
            failing,
            gas_price_wei,
            public_review_rpc,
            public_allowance_complete,
            math_deployed,
            fee_amount,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    /// Have Across keep `bps` of the amount from now on.
    fn set_across_fee_bps(&self, bps: u64) {
        self.across_fee_bps
            .store(bps, std::sync::atomic::Ordering::Relaxed);
    }

    /// Have Across hold each fee quote it is asked for from now on for `delay`.
    fn set_across_delay(&self, delay: Duration) {
        self.across_delay_ms.store(
            u64::try_from(delay.as_millis()).unwrap(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Have the providers answer 503 to paths starting with one of `prefixes` from now on.
    fn set_failing(&self, prefixes: &[&'static str]) {
        *self.failing.lock().unwrap() = prefixes.to_vec();
    }

    /// Have the RPC report `wei` as its gas price from now on.
    fn set_gas_price_wei(&self, wei: u64) {
        self.gas_price_wei
            .store(wei, std::sync::atomic::Ordering::Relaxed);
    }

    /// Have the orderbook quote `fee` of the sell amount as its network fee from now on.
    fn set_fee_amount(&self, fee: u64) {
        self.fee_amount
            .store(fee, std::sync::atomic::Ordering::Relaxed);
    }

    fn rpc(&self) -> reqwest::Url {
        self.url.join("rpc").unwrap()
    }

    /// Public previews read an existing allowance, whether the `CoW` proxy is deployed, and
    /// the math contract's code.
    fn enable_public_reviews(&self) {
        self.public_review_rpc
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Have the chain answer a read of the math contract's code with its runtime code, as it
    /// does at first, or with none, from now on.
    fn set_math_deployed(&self, deployed: bool) {
        self.math_deployed
            .store(deployed, std::sync::atomic::Ordering::Relaxed);
    }

    fn orderbook(&self) -> reqwest::Url {
        self.url.join("mainnet").unwrap()
    }

    /// The bodies of the quote requests so far.
    fn quotes(&self) -> Vec<serde_json::Value> {
        self.quotes.lock().unwrap().clone()
    }

    /// The paths of the bridge provider requests so far.
    fn bridge_requests(&self) -> Vec<String> {
        self.bridge_requests.lock().unwrap().clone()
    }

    /// The paths of the orderbook's order reports so far.
    fn order_requests(&self) -> Vec<String> {
        self.order_requests.lock().unwrap().clone()
    }

    /// How many RPC requests arrived on `path` so far.
    fn rpc_requests_on(&self, path: &str) -> usize {
        let requests = self.requests.lock().unwrap();
        requests
            .iter()
            .filter(|(request, _)| request == path)
            .count()
    }

    /// How many requests arrived so far, to tell later ones apart.
    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// The `x-route` tags of the requests after the first `from` whose path starts with
    /// `prefix`, each once.
    fn routes_since(&self, from: usize, prefix: &str) -> BTreeSet<Option<String>> {
        self.requests.lock().unwrap()[from..]
            .iter()
            .filter(|(path, _)| path.starts_with(prefix))
            .map(|(_, route)| route.clone())
            .collect()
    }
}

impl Drop for SwapStubs {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Answer one request, then close the connection.
async fn stub_response(
    stream: tokio::net::TcpStream,
    quotes: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    bridge_requests: Arc<std::sync::Mutex<Vec<String>>>,
    order_requests: Arc<std::sync::Mutex<Vec<String>>>,
    requests: StubRequests,
    bridge: StubBridge,
    gas_price_wei: Arc<std::sync::atomic::AtomicU64>,
    fee_amount: Arc<std::sync::atomic::AtomicU64>,
) {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = tokio::io::BufReader::new(stream);
    let mut request_line = String::new();
    if stream.read_line(&mut request_line).await.unwrap_or(0) == 0 {
        return;
    }
    let mut length = 0;
    let mut route = None;
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        let line = line.to_ascii_lowercase();
        if let Some(value) = line.strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
        if let Some(value) = line.strip_prefix("x-route:") {
            route = Some(value.trim().to_owned());
        }
    }
    let mut body = vec![0; length];
    if stream.read_exact(&mut body).await.is_err() {
        return;
    }
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let path = request_line.split(' ').nth(1).unwrap_or_default();
    requests.lock().unwrap().push((path.to_owned(), route));
    let failing = bridge
        .failing
        .lock()
        .unwrap()
        .iter()
        .any(|prefix| path.starts_with(prefix));
    let status = if failing {
        "503 Service Unavailable"
    } else {
        "200 OK"
    };
    let reply = if path.starts_with("/across/") || path.starts_with("/near/") {
        bridge_requests.lock().unwrap().push(path.to_owned());
        if path.starts_with("/across/suggested-fees") {
            let delay = bridge
                .across_delay_ms
                .load(std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        stub_bridge_reply(path, &body, &bridge)
    } else if request_line.contains("/api/v1/quote") {
        // Price the sell token so that one buy-token base unit is worth one wei, which keeps
        // the hook allowance far below the quoted output for any sell amount.
        #[allow(clippy::cast_precision_loss)]
        let sell_token_price = body["sellAmountBeforeFee"]
            .as_str()
            .and_then(|amount| amount.parse::<f64>().ok())
            .map_or_else(
                || "1".to_owned(),
                |amount| (STUB_BUY_AMOUNT as f64 / amount).to_string(),
            );
        // The network fee comes out of the sell amount, as `CoW` quotes a sell order.
        let fee = U256::from(fee_amount.load(std::sync::atomic::Ordering::Relaxed));
        let sell_amount = body["sellAmountBeforeFee"]
            .as_str()
            .and_then(|amount| amount.parse::<U256>().ok())
            .map_or_else(
                || body["sellAmountBeforeFee"].clone(),
                |amount| amount.saturating_sub(fee).to_string().into(),
            );
        let reply = serde_json::json!({
            "quote": {
                "sellToken": body["sellToken"], "buyToken": body["buyToken"],
                "sellAmount": sell_amount,
                "buyAmount": STUB_BUY_AMOUNT.to_string(), "validTo": 1,
                "feeAmount": fee.to_string(),
                "gasAmount": "0", "gasPrice": "0", "sellTokenPrice": sell_token_price, "kind": "sell",
                "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        });
        quotes.lock().unwrap().push(body);
        reply
    } else if path.contains("/api/v1/orders/") {
        order_requests.lock().unwrap().push(path.to_owned());
        serde_json::json!({
            "status": "fulfilled",
            "executedFee": STUB_EXECUTED_FEE.to_string(),
            "executedFeeToken": Address::repeat_byte(2),
        })
    } else if bridge
        .public_review_rpc
        .load(std::sync::atomic::Ordering::Relaxed)
        && body["method"] == "eth_call"
    {
        let allowance = body["params"][0]["input"]
            .as_str()
            .or_else(|| body["params"][0]["data"].as_str())
            .is_some_and(|input| input.starts_with("0xdd62ed3e"));
        let amount = if allowance
            && bridge
                .public_allowance_complete
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            U256::MAX
        } else {
            U256::ZERO
        };
        serde_json::json!({"jsonrpc": "2.0", "id": body["id"],
            "result": format!("0x{}", alloy::hex::encode(amount.to_be_bytes::<32>()))})
    } else if bridge
        .public_review_rpc
        .load(std::sync::atomic::Ordering::Relaxed)
        && body["method"] == "eth_getCode"
    {
        // Only the math contract has code: the account's `CoW` proxy isn't deployed.
        let math = body["params"][0]
            .as_str()
            .and_then(|address| address.parse::<Address>().ok())
            == Some(SWAP_MATH_ADDRESS)
            && bridge
                .math_deployed
                .load(std::sync::atomic::Ordering::Relaxed);
        let code = if math {
            &SWAP_MATH_CREATION_CODE[30..]
        } else {
            &[]
        };
        serde_json::json!({"jsonrpc": "2.0", "id": body["id"],
            "result": alloy::hex::encode_prefixed(code)})
    } else if body["method"] == "eth_gasPrice" {
        let price = gas_price_wei.load(std::sync::atomic::Ordering::Relaxed);
        serde_json::json!({"jsonrpc": "2.0", "id": body["id"], "result": format!("{price:#x}")})
    } else {
        serde_json::json!({
            "jsonrpc": "2.0", "id": body["id"],
            "error": {"code": -32601, "message": "unavailable in tests"}
        })
    };
    let reply = reply.to_string();
    let _ = stream
        .get_mut()
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .as_bytes(),
        )
        .await;
}

/// Mainnet USDT and Polygon's USDC and USDT, all in the default token list.
const STUB_USDT: Address = alloy::primitives::address!("dac17f958d2ee523a2206206994597c13d831ec7");
const STUB_POLYGON_USDC: Address =
    alloy::primitives::address!("3c499c542cef5e3811e1192ce70d8cc03d5c3359");
const STUB_POLYGON_USDT: Address =
    alloy::primitives::address!("c2132d05d31c914a87c6611c10748aeb04b58e8f");

/// WETH on Ethereum, and on Arbitrum One and Base, where it is the wrapped native token.
const STUB_WETH: Address = alloy::primitives::address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
const STUB_ARBITRUM_WETH: Address =
    alloy::primitives::address!("82af49447d8a07e3bd95bd0d56f35241523fbab1");
const STUB_BASE_WETH: Address =
    alloy::primitives::address!("4200000000000000000000000000000000000006");

/// The mints 1Click's stub lists on Solana: USDC's, and a look-alike sUSDC's.
const STUB_SOLANA_USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const STUB_SOLANA_SUSDC: &str = "FiNPw48pLZgbwmFiaYdTyJDvQCsefYx3CsBy4iVZFpBz";
/// A Solana receiver: 32 bytes in base58, which no wallet holds.
const SOLANA_RECEIVER: &str = "9jQtwHhZT1H2TYSMt74msmBmy8UPen4GUysNynPUVkkv";

/// The stub providers' lists from Ethereum. Across bridges USDC and USDT to Polygon, and WETH
/// to Arbitrum One and Base. 1Click lists both stablecoins on Ethereum and Polygon, native POL on
/// Polygon, native ETH on Arbitrum One, SOL, USDC and sUSDC on Solana, and BTC on Bitcoin.
fn stub_bridge_list(path: &str) -> serde_json::Value {
    let route = |chain: u64, origin: Address, destination: Address, symbol: &str| {
        serde_json::json!({
            "originChainId": 1, "originToken": origin, "destinationChainId": chain,
            "destinationToken": destination, "originTokenSymbol": symbol,
            "destinationTokenSymbol": symbol, "isNative": false
        })
    };
    let token = |blockchain: &str, symbol: &str, contract: Option<Address>| {
        serde_json::json!({
            "assetId": format!("nep141:{blockchain}-{symbol}"), "decimals": 6,
            "blockchain": blockchain, "symbol": symbol, "contractAddress": contract
        })
    };
    if path.starts_with("/across/available-routes") {
        serde_json::json!([
            route(137, STUB_USDC, STUB_POLYGON_USDC, "USDC"),
            route(137, STUB_USDT, STUB_POLYGON_USDT, "USDT"),
            route(42161, STUB_WETH, STUB_ARBITRUM_WETH, "WETH"),
            route(8453, STUB_WETH, STUB_BASE_WETH, "WETH"),
        ])
    } else if path.starts_with("/near/v0/tokens") {
        // Non-EVM contracts are 1Click's strings, such as a Solana mint.
        let non_evm = |blockchain: &str, symbol: &str, contract: Option<&str>, decimals: u8| {
            serde_json::json!({
                "assetId": format!("nep141:{blockchain}-{symbol}"), "decimals": decimals,
                "blockchain": blockchain, "symbol": symbol, "contractAddress": contract
            })
        };
        serde_json::json!([
            token("eth", "USDC", Some(STUB_USDC)),
            token("eth", "USDT", Some(STUB_USDT)),
            token("pol", "POL", None),
            token("pol", "USDC", Some(STUB_POLYGON_USDC)),
            token("pol", "USDT", Some(STUB_POLYGON_USDT)),
            token("arb", "ETH", None),
            non_evm("sol", "SOL", None, 9),
            non_evm("sol", "USDC", Some(STUB_SOLANA_USDC), 6),
            non_evm("sol", "sUSDC", Some(STUB_SOLANA_SUSDC), 6),
            non_evm("btc", "BTC", None, 8),
            non_evm("zec", "ZEC", None, 8),
        ])
    } else {
        serde_json::json!({})
    }
}

/// The seed of the key that stands in for 1Click's quote signer.
const STUB_QUOTE_SEED: u8 = 7;
/// What 1Click's stub quotes deliver, in base units of the destination token: about 12.6 and
/// at least 12.5 of an 18-decimal token.
const STUB_NEAR_EXPECTED: &str = "12600000000000000000";
const STUB_NEAR_MINIMUM: &str = "12500000000000000000";
/// 1Click's USD value of the expected output, $12.60, which values the minimum at $12.50.
const STUB_NEAR_EXPECTED_USD: &str = "12.6";

/// A stub provider's answer. Across quotes a fee of the configured share of the amount; 1Click
/// signs a dry quote of [`STUB_NEAR_EXPECTED`] and [`STUB_NEAR_MINIMUM`] for the request it was
/// sent with the stand-in key, and reports every deposit delivered with [`STUB_NEAR_EXPECTED`].
/// Other requests get the lists of [`stub_bridge_list`].
fn stub_bridge_reply(
    path: &str,
    body: &serde_json::Value,
    bridge: &StubBridge,
) -> serde_json::Value {
    if path.starts_with("/across/suggested-fees") {
        let amount = reqwest::Url::parse(&format!("http://stub{path}"))
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "amount")
                    .and_then(|(_, amount)| amount.parse::<U256>().ok())
            })
            .unwrap_or_default();
        let bps = bridge
            .across_fee_bps
            .load(std::sync::atomic::Ordering::Relaxed);
        let fee = amount * U256::from(bps) / U256::from(10_000_u32);
        serde_json::json!({
            "outputAmount": (amount - fee).to_string(),
            "totalRelayFee": {"pct": "0", "total": fee.to_string()},
            "relayerGasFee": {
                "pct": (U256::from(bridge.across_relayer_gas_bps.load(std::sync::atomic::Ordering::Relaxed))
                    * U256::from(100_000_000_000_000_u64)).to_string(),
                "total": "0"
            },
            "lpFee": {"pct": "0", "total": "0"},
            "timestamp": "1790718359", "fillDeadline": "1790725559",
            "exclusiveRelayer": Address::ZERO, "exclusivityDeadline": 0,
            "spokePoolAddress": bridge.spoke_pool,
            "destinationSpokePoolAddress": Address::repeat_byte(0x55),
            "isAmountTooLow": false,
            "limits": {"minDeposit": "1", "maxDeposit": U256::MAX.to_string()},
            "estimatedFillTimeSec": 2
        })
    } else if path.starts_with("/near/v0/quote") {
        let response = serde_json::json!({
            "quote": {
                "amountIn": body["amount"], "minAmountIn": body["amount"],
                "amountOut": STUB_NEAR_EXPECTED, "minAmountOut": STUB_NEAR_MINIMUM,
                "amountOutUsd": STUB_NEAR_EXPECTED_USD, "timeEstimate": 20
            },
            "quoteRequest": body,
            "timestamp": "2026-09-30T00:00:00.000Z"
        });
        serde_json::from_str(&wallet_ops::bridge::sign_quote_response_with_stand_in(
            response,
            STUB_QUOTE_SEED,
        ))
        .unwrap()
    } else if path.starts_with("/near/v0/status") {
        serde_json::json!({
            "status": "SUCCESS",
            "swapDetails": {
                "amountOut": STUB_NEAR_EXPECTED,
                "destinationChainTxHashes": [
                    {"hash": alloy::primitives::B256::repeat_byte(0x5c), "explorerUrl": ""}
                ]
            }
        })
    } else {
        stub_bridge_list(path)
    }
}

/// Bridge clients for the stub providers, on `orderbook`'s route.
fn stub_bridge_clients(stubs: &SwapStubs, orderbook: &CowOrderbookClient) -> SwapBridgeClients {
    SwapBridgeClients {
        across: wallet_ops::bridge::AcrossClient::new(
            orderbook.http().clone(),
            stubs.url.join("across").unwrap(),
        )
        .unwrap(),
        near: wallet_ops::bridge::NearIntentsClient::new(
            orderbook.http().clone(),
            stubs.url.join("near").unwrap(),
            &wallet_ops::bridge::stand_in_quote_key(STUB_QUOTE_SEED),
        )
        .unwrap(),
    }
}

/// A new swap of 1 USDC, planned from a 10 USDC stub note, to `Address::repeat_byte(4)`, with
/// the Buy picker open on Polygon once the stub providers' routes are listed. Picking a token
/// there makes it a Bridge swap. Polygon is enabled with the stub's RPC.
fn open_bridge_form(
    root: &Entity<WalletRoot>,
    swaps: &Entity<PrivateSwapsView>,
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
    cx: &mut gpui::VisualTestContext,
) {
    let orderbook = stub_orderbook(stubs, runtime);
    cx.update(|window, cx| {
        root.update(cx, |root, _| {
            root.effective_token_registry = wallet_ops::settings::build_effective_token_registry(
                &wallet_ops::settings::WalletSettings::default(),
            )
            .unwrap();
            enable_stub_chain(root, stubs, 137);
        });
        swaps.update(cx, |swaps, cx| {
            swaps
                .private_owner()
                .unwrap()
                .plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10_000_000));
            swaps.open_form(
                None,
                STUB_USDC,
                None,
                Some(U256::from(1_000_000)),
                None,
                SwapDelivery::Reshield,
                window,
                cx,
            );
            let form = swaps.form.as_mut().unwrap();
            form.bridge_clients = Some(stub_bridge_clients(stubs, &orderbook));
            form.orderbook = Some(orderbook);
            swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
            set_receiver_text(swaps, &Address::repeat_byte(4).to_string(), window, cx);
            swaps.open_buy_picker(window, cx);
            swaps.show_buy_picker_network(137, window, cx);
        });
    });
    drive_until(cx, runtime, |cx| {
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            form.bridge.routes.contains_key(&(STUB_USDC, 137))
        })
    });
}

/// Start Polygon in the same wallet scope as the origin fixture and expose its ready session.
fn start_polygon_session(
    root: &Entity<WalletRoot>,
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
    store: &wallet_ops::WalletSessionStore,
    cx: &mut gpui::VisualTestContext,
) -> Arc<WalletSession> {
    let mut lifecycle = WalletSyncLifecycle::new();
    let registration = lifecycle.prepare_startup(137);
    let (request, http) = root.read_with(cx, |root, _| {
        let mut chain = root.effective_chain_configs.get(137).unwrap().clone();
        let private = chain.railgun.as_mut().unwrap();
        private.archive_rpc_url = None;
        private.sync.quick_sync_endpoint = None;
        private.sync.indexed_artifact_source = None;
        let poi = wallet_ops::PoiReadSource::PoiProxy {
            rpc_url: stubs.rpc().into(),
        };
        let request = wallet_ops::ViewWalletChainSessionRequest {
            view_session: root.view_session.clone().unwrap(),
            wallet_scope_generation: registration.generation,
            chain_id: 137,
            effective_chain: chain,
            sync_start_policy: wallet_ops::DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
            init_block_number: Some(0),
            sync_to_block: Some(0),
            use_indexed_wallet_catch_up: false,
            poi_read_source: poi,
            rewind_wallet_cache: false,
            progress_tx: None,
        };
        (request, root.http.clone())
    });
    let session = Arc::new(
        runtime
            .block_on(store.start_view_wallet_session_immediate(request, &http))
            .unwrap(),
    );
    let observation = session.observation_rx.borrow().clone();
    cx.update(|_, cx| {
        root.update(cx, |root, _| {
            root.chain_states.insert(
                137,
                ChainUtxoState::Ready {
                    session: Arc::clone(&session),
                    snapshot: observation.snapshot,
                    observer_token: registration.observer_token,
                    sync_tip: wallet_ops::WalletSyncTip::default(),
                    poi_refreshing: false,
                    ppoi_workflow_status: observation.ppoi_workflow_status,
                },
            );
        });
    });
    session
}

/// The built-in `chain_id` as the default settings configure it.
fn built_in_chain(chain_id: u64) -> EffectiveChainConfig {
    wallet_ops::settings::build_effective_chain_configs(
        &wallet_ops::settings::WalletSettings::default(),
    )
    .unwrap()
    .get(chain_id)
    .unwrap()
    .clone()
}

/// Put `chain` among the wallet's chains, in place of one with its id.
fn put_chain(root: &mut WalletRoot, chain: EffectiveChainConfig) {
    let chain_id = chain.chain_id;
    root.effective_chain_configs = root
        .effective_chain_configs
        .clone()
        .into_values()
        .filter(|known| known.chain_id != chain_id)
        .chain(std::iter::once(chain))
        .collect();
}

/// Enable the built-in `chain_id` with the stub's RPC.
fn enable_stub_chain(root: &mut WalletRoot, stubs: &SwapStubs, chain_id: u64) {
    let mut enabled = built_in_chain(chain_id);
    enabled.enabled = true;
    enabled.rpc_route = wallet_ops::RpcChainRoute::new(chain_id, vec![stubs.rpc()]);
    put_chain(root, enabled);
}

/// An orderbook client for the stub orderbook, on a direct route.
fn stub_orderbook(stubs: &SwapStubs, runtime: &tokio::runtime::Runtime) -> CowOrderbookClient {
    orderbook_at(stubs.orderbook(), runtime)
}

/// An orderbook client for `url`, on a direct route.
fn orderbook_at(url: reqwest::Url, runtime: &tokio::runtime::Runtime) -> CowOrderbookClient {
    let directory = tempfile::tempdir().unwrap();
    runtime.block_on(async {
        let http = wallet_ops::build_wallet_network_context(wallet_ops::WalletNetworkConfig {
            network_mode: Some(wallet_ops::WalletNetworkMode::Direct),
            proxy: None,
            data_dir: directory.path(),
        })
        .await
        .unwrap();
        CowOrderbookClient::new(http.operation_http_client().await.unwrap(), url, 1).unwrap()
    })
}

/// Run the fixture's runtime, where quotes make their requests, and the UI, including the
/// quote debounce, until `done`.
fn drive_until(
    cx: &mut gpui::VisualTestContext,
    runtime: &tokio::runtime::Runtime,
    mut done: impl FnMut(&mut gpui::VisualTestContext) -> bool,
) {
    for _ in 0..500 {
        cx.executor().advance_clock(QUOTE_DEBOUNCE);
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        runtime.block_on(tokio::time::sleep(Duration::from_millis(10)));
    }
    panic!("the swap's requests didn't finish");
}

/// Direct-mode wallet HTTP whose every operation client tags its requests with an `x-route`
/// header of its own, which [`SwapStubs`] records.
fn tagged_operation_http() -> wallet_ops::HttpContext {
    let opened = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    wallet_ops::HttpContext::with_rpc_client_for_tests(
        reqwest::Client::new(),
        wallet_ops::WalletNetworkMode::Direct,
    )
    .with_operation_clients_for_tests(move || {
        let tag = opened.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-route", format!("route-{tag}").parse()?);
        Ok(wallet_ops::OperationHttpClient::for_tests(
            reqwest::Client::builder()
                .default_headers(headers)
                .build()?,
            wallet_ops::OperationNetworkIsolation::Unavailable(
                wallet_ops::WalletNetworkMode::Direct,
            ),
        ))
    })
}

/// The form's quote, once it's ready.
fn ready_review(
    swaps: &Entity<PrivateSwapsView>,
    runtime: &tokio::runtime::Runtime,
    cx: &mut gpui::VisualTestContext,
) -> Arc<SwapReview> {
    drive_until(cx, runtime, |cx| {
        swaps.read_with(cx, |swaps, _| {
            !matches!(
                swaps.form.as_ref().map(|form| &form.quote),
                Some(QuoteState::Loading)
            )
        })
    });
    swaps.read_with(cx, |swaps, _| match &swaps.form.as_ref().unwrap().quote {
        QuoteState::Ready(review) => Arc::clone(review),
        QuoteState::Failed(error) => panic!("{error:#}"),
        _ => panic!("the quote isn't ready"),
    })
}

/// The form's quote once no bridge refresh is under way.
fn refreshed_review(
    swaps: &Entity<PrivateSwapsView>,
    runtime: &tokio::runtime::Runtime,
    cx: &mut gpui::VisualTestContext,
) -> Arc<SwapReview> {
    drive_until(cx, runtime, |cx| {
        swaps.read_with(cx, |swaps, _| {
            swaps.form.as_ref().unwrap().quote_task.is_none()
        })
    });
    ready_review(swaps, runtime, cx)
}

/// The amounts of the Across fee quotes asked for so far.
fn across_fee_amounts(stubs: &SwapStubs) -> Vec<U256> {
    stubs
        .bridge_requests()
        .iter()
        .filter(|path| path.starts_with("/across/suggested-fees"))
        .map(|path| {
            reqwest::Url::parse(&format!("http://stub{path}"))
                .unwrap()
                .query_pairs()
                .find(|(key, _)| key == "amount")
                .unwrap()
                .1
                .parse()
                .unwrap()
        })
        .collect()
}

/// The swap view after a restart, which knows only the saved records. It keeps the setup's
/// observation, quotes through the stub orderbook and plans from 10 USDC.
fn restarted_swaps(
    root: &Entity<WalletRoot>,
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
    operation: ExecutorOperationId,
    setup: wallet_ops::SwapSetupStatus,
    cx: &mut gpui::VisualTestContext,
) -> Entity<PrivateSwapsView> {
    let orderbook = stub_orderbook(stubs, runtime);
    cx.update(|window, cx| {
        let swaps = root.update(cx, |root, cx| {
            root.effective_token_registry = wallet_ops::settings::build_effective_token_registry(
                &wallet_ops::settings::WalletSettings::default(),
            )
            .unwrap();
            root.clear_private_swaps(cx);
            root.ensure_private_swaps(window, cx);
            root.private_swaps_view().unwrap()
        });
        swaps.update(cx, |swaps, _| {
            swaps
                .private_owner()
                .unwrap()
                .plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10_000_000));
            let tracking = swaps.tracking.entry(operation).or_default();
            tracking.setup = Some(setup);
            tracking.orderbook = Some(orderbook);
            assert_eq!(
                swaps.stage(swaps.record(operation).unwrap()),
                SwapStage::Approved
            );
        });
        swaps
    })
}

/// A new swap of 1 USDC for USDT to the private balance, planned from a 10 USDC stub note and
/// quoted through the stub orderbook.
fn open_private_form(
    root: &Entity<WalletRoot>,
    swaps: &Entity<PrivateSwapsView>,
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
    cx: &mut gpui::VisualTestContext,
) {
    let orderbook = stub_orderbook(stubs, runtime);
    cx.update(|window, cx| {
        root.update(cx, |root, _| {
            root.effective_token_registry = wallet_ops::settings::build_effective_token_registry(
                &wallet_ops::settings::WalletSettings::default(),
            )
            .unwrap();
        });
        swaps.update(cx, |swaps, cx| {
            swaps
                .private_owner()
                .unwrap()
                .plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10_000_000));
            swaps.open_form(
                None,
                STUB_USDC,
                Some(STUB_USDT),
                Some(U256::from(1_000_000)),
                None,
                SwapDelivery::Reshield,
                window,
                cx,
            );
            swaps.form.as_mut().unwrap().orderbook = Some(orderbook);
            swaps.schedule_quote(window, cx);
        });
    });
}

/// Emit the gas slider's `Change` to `value`, or its `Release` when `release`, as a drag does.
fn move_gas_slider(
    swaps: &Entity<PrivateSwapsView>,
    value: f32,
    release: bool,
    cx: &mut gpui::VisualTestContext,
) {
    use gpui_component::slider::SliderValue;
    cx.update(|_, cx| {
        let slider = swaps.read(cx).form.as_ref().unwrap().gas_slider.clone();
        slider.update(cx, |_, cx| {
            cx.emit(if release {
                SliderEvent::Release(SliderValue::Single(value))
            } else {
                SliderEvent::Change(SliderValue::Single(value))
            });
        });
    });
    cx.run_until_parked();
}

/// The review's Gas row: the gas the user pays of the gas estimate, in money.
fn gas_row(swaps: &PrivateSwapsView, review: &SwapReview, cx: &App) -> (String, String) {
    let buy = review.plan().buy_token();
    (
        "Gas".to_owned(),
        format!(
            "{} · up to {} of ≈ {}",
            gas_share_name(review.gas_share_bps()),
            swaps.gas_money(buy, review.gas_allowance(), cx),
            swaps.gas_money(buy, review.gas_estimate(), cx)
        ),
    )
}

fn configure_public_review_assets(root: &mut WalletRoot) {
    for account in &mut Arc::make_mut(root.public_balance_snapshot.as_mut().unwrap()).accounts {
        for balance in &mut account.balances {
            let token = match balance.asset.id {
                wallet_ops::PublicAssetId::Erc20(token) if token == Address::repeat_byte(42) => {
                    Some((STUB_USDC, "USDC"))
                }
                wallet_ops::PublicAssetId::Erc20(token) if token == Address::repeat_byte(43) => {
                    Some((STUB_USDT, "USDT"))
                }
                _ => None,
            };
            if let Some((token, symbol)) = token {
                balance.asset.id = wallet_ops::PublicAssetId::Erc20(token);
                balance.asset.symbol = symbol.into();
                balance.asset.decimals = 6;
                balance.amount = wallet_ops::PublicBalanceAmount::Available(U256::from(10_000_000));
            }
        }
    }
}

/// Build the backend's actual deposit or order review; only the network responses are stubbed.
#[allow(clippy::too_many_arguments)]
fn public_review_fixture(
    root: &Entity<WalletRoot>,
    session: &Arc<WalletSession>,
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
    sell: Address,
    sell_amount: U256,
    order: bool,
    cx: &gpui::VisualTestContext,
) -> (
    wallet_ops::PublicSwapReview,
    wallet_ops::bridge::AcrossClient,
    CowOrderbookClient,
    wallet_ops::bridge::PublicBridgeDestination,
) {
    use wallet_ops::bridge::{
        AcrossClient, AcrossRoute, PublicSellAsset, public_across_destination_tokens,
    };
    let orderbook = stub_orderbook(stubs, runtime);
    let across =
        AcrossClient::new(orderbook.http().clone(), stubs.url.join("across").unwrap()).unwrap();
    let (origin, registry, source) = root.read_with(cx, |root, _| {
        (
            root.effective_chain_configs.get(1).unwrap().clone(),
            root.effective_token_registry.clone(),
            root.public_accounts[1].address,
        )
    });
    let route = public_across_destination_tokens(
        &[AcrossRoute {
            origin_token: if order { STUB_USDC } else { sell },
            destination_token: STUB_POLYGON_USDC,
            origin_symbol: "USDC".into(),
            destination_symbol: "USDC".into(),
        }],
        PublicSellAsset::Erc20(sell),
        true,
        &registry,
        1,
        137,
    )
    .remove(0);
    let review = runtime
        .block_on(session.executor_owner().unwrap().review_public_swap(
            wallet_ops::PublicSwapReviewRequest {
                origin: &origin,
                source,
                sell: PublicSellAsset::Erc20(sell),
                sell_amount,
                destination: &route,
                slippage_bps: DEFAULT_SLIPPAGE_BPS,
                gas_share_bps: GAS_SHARE_BALANCED_BPS,
                on_shield_failure: BridgeShieldFailure::default(),
                orderbook: Some(&orderbook),
                across: &across,
                anchor_cache: None,
                token_registry: &registry,
                anchor_deviation_bps: 300,
                max_fee_per_gas: 1,
                max_priority_fee_per_gas: 1,
                client: orderbook.http(),
            },
        ))
        .unwrap();
    (review, across, orderbook, route)
}

/// Select through the production menu so disabled rows and owner subscriptions are exercised.
fn choose_swap_source(cx: &mut gpui::VisualTestContext, selector: &'static str) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    if cx.debug_bounds(selector).is_none() {
        let trigger = cx
            .debug_bounds("swap-pay-from")
            .unwrap_or_else(|| panic!("Pay from control before {selector}"));
        cx.simulate_click(trigger.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
    let row = cx.debug_bounds(selector).expect("source menu row");
    cx.simulate_click(row.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

/// Click the rendered Pay from control and report whether its menu opened.
fn pay_from_menu_opens(cx: &mut gpui::VisualTestContext) -> bool {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let trigger = cx.debug_bounds("swap-pay-from").expect("Pay from control");
    cx.simulate_click(trigger.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.debug_bounds("swap-source-private").is_some()
}

#[path = "ui_tests/bridge_restarts.rs"]
mod bridge_restarts;
#[path = "ui_tests/bridge_reviews.rs"]
mod bridge_reviews;
#[path = "ui_tests/bridge_routes.rs"]
mod bridge_routes;
#[path = "ui_tests/bridge_setup_progress.rs"]
mod bridge_setup_progress;
#[path = "ui_tests/costs.rs"]
mod costs;
#[path = "ui_tests/fills.rs"]
mod fills;
#[path = "ui_tests/manual_checks.rs"]
mod manual_checks;
#[path = "ui_tests/observation_routes.rs"]
mod observation_routes;
#[path = "ui_tests/orders.rs"]
mod orders;
#[path = "ui_tests/pickers.rs"]
mod pickers;
#[path = "ui_tests/private_bridge_accounts.rs"]
mod private_bridge_accounts;
#[path = "ui_tests/public_orders.rs"]
mod public_orders;
#[path = "ui_tests/public_preparation.rs"]
mod public_preparation;
#[path = "ui_tests/public_review.rs"]
mod public_review;
#[path = "ui_tests/public_sources.rs"]
mod public_sources;
#[path = "ui_tests/quotes.rs"]
mod quotes;
#[path = "ui_tests/receivers.rs"]
mod receivers;
#[path = "ui_tests/refunds_and_deliveries.rs"]
mod refunds_and_deliveries;
#[path = "ui_tests/setup_inclusion.rs"]
mod setup_inclusion;
#[path = "ui_tests/setup_retries.rs"]
mod setup_retries;
#[path = "ui_tests/setup_retry_reviews.rs"]
mod setup_retry_reviews;
#[path = "ui_tests/speed_up.rs"]
mod speed_up;
#[path = "ui_tests/speed_up_form.rs"]
mod speed_up_form;
#[path = "ui_tests/swap_detail.rs"]
mod swap_detail;
