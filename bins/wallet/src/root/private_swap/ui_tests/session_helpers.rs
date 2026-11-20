use super::*;

/// A new swap of 1 USDC, planned from a 10 USDC stub note, to `Address::repeat_byte(4)`, with
/// the Buy picker open on Polygon once the stub providers' routes are listed. Picking a token
/// there makes it a Bridge swap. Polygon is enabled with the stub's RPC.
pub(super) fn open_bridge_form(
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
pub(super) fn start_polygon_session(
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
pub(super) fn built_in_chain(chain_id: u64) -> EffectiveChainConfig {
    wallet_ops::settings::build_effective_chain_configs(
        &wallet_ops::settings::WalletSettings::default(),
    )
    .unwrap()
    .get(chain_id)
    .unwrap()
    .clone()
}

/// Put `chain` among the wallet's chains, in place of one with its id.
pub(super) fn put_chain(root: &mut WalletRoot, chain: EffectiveChainConfig) {
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
pub(super) fn enable_stub_chain(root: &mut WalletRoot, stubs: &SwapStubs, chain_id: u64) {
    let mut enabled = built_in_chain(chain_id);
    enabled.enabled = true;
    enabled.rpc_route = wallet_ops::RpcChainRoute::new(chain_id, vec![stubs.rpc()]);
    put_chain(root, enabled);
}

/// An orderbook client for the stub orderbook, on a direct route.
pub(super) fn stub_orderbook(
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
) -> CowOrderbookClient {
    orderbook_at(stubs.orderbook(), runtime)
}

/// An orderbook client for `url`, on a direct route.
pub(super) fn orderbook_at(
    url: reqwest::Url,
    runtime: &tokio::runtime::Runtime,
) -> CowOrderbookClient {
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
pub(super) fn drive_until(
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
pub(super) fn tagged_operation_http() -> wallet_ops::HttpContext {
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
pub(super) fn ready_review(
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
pub(super) fn refreshed_review(
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
pub(super) fn across_fee_amounts(stubs: &SwapStubs) -> Vec<U256> {
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
pub(super) fn restarted_swaps(
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
pub(super) fn open_private_form(
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
pub(super) fn move_gas_slider(
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
pub(super) fn gas_row(swaps: &PrivateSwapsView, review: &SwapReview, cx: &App) -> (String, String) {
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

pub(super) fn configure_public_review_assets(root: &mut WalletRoot) {
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
pub(super) fn public_review_fixture(
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
pub(super) fn choose_swap_source(cx: &mut gpui::VisualTestContext, selector: &'static str) {
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
pub(super) fn pay_from_menu_opens(cx: &mut gpui::VisualTestContext) -> bool {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let trigger = cx.debug_bounds("swap-pay-from").expect("Pay from control");
    cx.simulate_click(trigger.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.debug_bounds("swap-source-private").is_some()
}
