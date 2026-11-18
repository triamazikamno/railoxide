use super::*;

impl PrivateSwapsView {
    /// Continue or retry a swap. A retired setup starts a new review with a fresh account. The
    /// swap's delivery comes back with its tokens: approved, or the retried attempt's, which a
    /// retry keeps. A new swap, from a retired setup or a new order, can change it.
    pub(in crate::root::private_swap) fn open_existing_form(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Debug UI fixture: a synthesized record has no swap to continue or retry.
        if crate::root::private_swap::ui_fixture::holds_swaps()
            || !self.session_is_current(cx)
            || self.busy()
        {
            return;
        }
        let Some(record) = self.record(operation) else {
            return;
        };
        // An included setup can't be sent again, but its approved order can be reviewed.
        if self.setup_retry_problem(record).is_some() && self.approved_use(record).is_none() {
            return;
        }
        if let Some(pending) = self.pending_order(record) {
            self.open_form(
                Some(operation),
                pending.sell,
                Some(pending.buy),
                Some(pending.amount),
                Some(SavedTerms {
                    slippage_bps: Some(pending.slippage_bps),
                    gas_share_bps: Some(pending.gas_share_bps),
                    valid_for: Some(pending.valid_for),
                }),
                pending.delivery,
                window,
                cx,
            );
            if let Some(form) = self.form.as_mut() {
                form.back_to_detail = Some(operation);
                form.reuse_account = pending.reuse_account;
                form.reuse_use = pending.reuse_account.then_some(pending.swap_use);
            }
            // A reused account's draft names the destination account its swap use claimed.
            self.refresh_form_delivery(cx);
            self.refresh_setup_route(cx);
            self.schedule_quote(window, cx);
            return;
        }
        let Some((sell, buy)) = swap_tokens(record) else {
            return;
        };
        let tracking = self.tracking.get(&operation);
        let amount =
            swap_sell_amount(record).or_else(|| tracking.and_then(|tracking| tracking.amount));
        // The last order's terms, or the approved ones. An order or approval saved before gas
        // shares restores its tolerance, and the share and validity take their defaults.
        let bounds = record
            .swap()
            .and_then(|swap| swap.orders().last())
            .map(wallet_ops::vault::SwapOrderRecord::bounds)
            .or_else(|| record.swap_approval().map(|approval| &approval.bounds));
        let terms = match bounds {
            Some(bounds) => SavedTerms {
                slippage_bps: Some(bounds.slippage_bps),
                gas_share_bps: bounds.gas_share_bps,
                valid_for: bounds
                    .valid_for_secs
                    .map(|secs| Duration::from_secs(secs.into())),
            },
            None => SavedTerms {
                slippage_bps: tracking.and_then(|tracking| tracking.slippage_bps),
                gas_share_bps: tracking.and_then(|tracking| tracking.gas_share_bps),
                valid_for: tracking.and_then(|tracking| tracking.valid_for),
            },
        };
        let delivery = swap_delivery(record);
        let existing = (self.stage(record) != SwapStage::SetupRetired).then_some(operation);
        let prepared_use = existing.and_then(|_| {
            crate::root::private_swap::model::prepared_swap_use(record).map(SwapUseRecord::id)
        });
        self.open_form(
            existing,
            sell,
            Some(buy),
            amount,
            Some(terms),
            delivery,
            window,
            cx,
        );
        if let Some(form) = self.form.as_mut() {
            form.back_to_detail = Some(operation);
            form.reuse_use = prepared_use;
        }
    }

    /// Open the swap form on a speed-up of `operation`'s open order: a second order with a
    /// lower minimum, signed while that order stays live. The order's tokens, spend, delivery,
    /// price tolerance and validity fill the form and stay fixed, and a fresh quote is planned
    /// from that order's record. `false` when the order can't be sped up, and nothing opens.
    pub(in crate::root::private_swap) fn open_speed_up_form(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        // Debug UI fixture: a speed-up mode holds its form on a synthesized quote.
        #[cfg(debug_assertions)]
        if crate::root::private_swap::ui_fixture::speed_up_refusal() == Some(Ok(())) {
            self.open_ui_fixture_retry_review(window, cx);
            return true;
        }
        if crate::root::private_swap::ui_fixture::holds_swaps()
            || !self.session_is_current(cx)
            || self.busy()
        {
            return false;
        }
        let Some(record) = self.record(operation) else {
            return false;
        };
        // The owner's rule, as it answered for the records now shown. Planning asks it again.
        if self.speed_up_refusal(record).is_some() {
            return false;
        }
        let Some((swap, open)) = record
            .swap()
            .and_then(|swap| Some((swap, swap.orders().last()?)))
        else {
            return false;
        };
        let tokens = swap.order_terms(open);
        let (sell, buy) = (tokens.sell_token(), tokens.buy_token());
        let bounds = open.bounds();
        let terms = SavedTerms {
            slippage_bps: Some(bounds.slippage_bps),
            gas_share_bps: bounds.gas_share_bps,
            valid_for: bounds
                .valid_for_secs
                .map(|secs| Duration::from_secs(secs.into())),
        };
        let (amount, delivery) = (bounds.spend_amount(), open.delivery());
        let speed_up = SpeedUpOrder {
            uid: open.uid(),
            bounds: bounds.clone(),
        };
        self.open_form(
            Some(operation),
            sell,
            Some(buy),
            Some(amount),
            Some(terms),
            delivery,
            window,
            cx,
        );
        let Some(form) = self.form.as_mut() else {
            return false;
        };
        form.back_to_detail = Some(operation);
        form.speed_up = Some(speed_up);
        // The form opened as its placed swap's, which isn't quoted.
        self.schedule_quote(window, cx);
        true
    }

    /// Open the form with its fields filled in, and the order `terms` a reopened swap restores.
    /// A native `buy`, which only a Public address `delivery` receives, fills in as the wrapped
    /// native Buy asset with native output, before a quote is scheduled. A Bridge `delivery`
    /// fills in its network, provider, surplus choice and destination token, which replaces
    /// `buy`, the token handed to the provider. A private one is Private balance on its network.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn open_form(
        &mut self,
        operation: Option<ExecutorOperationId>,
        sell: Address,
        buy: Option<Address>,
        amount: Option<U256>,
        terms: Option<SavedTerms>,
        delivery: SwapDelivery,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_form_with_source(
            operation, sell, buy, amount, terms, delivery, None, window, cx,
        );
    }

    pub(in crate::root::private_swap) fn open_public_form(
        &mut self,
        account: PublicAccountMetadata,
        sell: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx) || self.busy() {
            return;
        }
        // Debug UI fixture: a `retry-review` or `split:` mode shows its private swap instead.
        #[cfg(debug_assertions)]
        if self.open_ui_fixture_swap(window, cx) {
            return;
        }
        #[cfg(debug_assertions)]
        let source = account.address;
        self.open_form_with_source(
            None,
            sell,
            None,
            None,
            None,
            SwapDelivery::Reshield,
            Some(account),
            window,
            cx,
        );
        // Debug UI fixture: a `detail:` mode shows its synthesized swap in place of the form.
        #[cfg(debug_assertions)]
        self.open_ui_fixture_detail(source, sell, window, cx);
        #[cfg(debug_assertions)]
        self.stage_public_route_error_for_fixture(window, cx);
    }

    /// Debug UI fixture: in a `quote-error` mode, open the private form of a new swap and hold it
    /// on the synthesized pricing failure. The fixture holds back the form's own quote and
    /// routes, so nothing replaces the failure.
    #[cfg(debug_assertions)]
    pub(in crate::root::private_swap) fn open_ui_fixture_quote_error(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx) || self.busy() {
            return;
        }
        let Some((sell, buy, amount, error)) = crate::root::private_swap::ui_fixture::quote_error()
        else {
            return;
        };
        self.open_form(
            None,
            sell,
            Some(buy),
            Some(amount),
            None,
            SwapDelivery::Reshield,
            window,
            cx,
        );
        if let Some(form) = self.form.as_mut() {
            form.quote = QuoteState::Failed(error);
        }
        cx.notify();
    }

    /// Debug UI fixture: in a `cancelled:` or `native-buy` mode, open the private form of a new
    /// swap of `sell` for `buy` over the records the mode staged, if any. The fixture holds back
    /// the form's own quote and routes, so the form has no quote.
    #[cfg(debug_assertions)]
    pub(in crate::root::private_swap) fn open_ui_fixture_new_swap(
        &mut self,
        sell: Address,
        buy: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_form(
            None,
            sell,
            Some(buy),
            None,
            None,
            SwapDelivery::Reshield,
            window,
            cx,
        );
    }

    /// Debug UI fixture: in a `retry-review` or speed-up mode, open the private form of the
    /// synthesized swap's retry or speed-up and hold it on the synthesized review. The
    /// orderbook client a review is approved with takes a route of its own, which is opened
    /// first and never asked anything.
    #[cfg(debug_assertions)]
    pub(in crate::root::private_swap) fn open_ui_fixture_retry_review(
        &self,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        let profile = self.root.upgrade().and_then(|root| {
            root.read(cx)
                .effective_chain_configs
                .get(self.origin_chain_id)
                .and_then(EffectiveChainConfig::swap_profile)
        });
        let (Some(owner), Some(profile), Some(http)) =
            (self.private_owner().cloned(), profile, self.http(cx))
        else {
            tracing::warn!(target: LOG_TARGET, "the debug UI fixture's review needs the network's private session");
            return;
        };
        let runtime = self.runtime.clone();
        cx.spawn_in(window, async move |view, cx| {
            let orderbook =
                runtime
                    .spawn(async move {
                        owner.swap_orderbook_client(&http.operation_http_client().await?)
                    })
                    .await;
            let _ = view.update_in(cx, |view, window, cx| {
                if let Ok(Ok(orderbook)) = orderbook {
                    view.stage_ui_fixture_retry_review(&profile, orderbook, window, cx);
                } else {
                    tracing::warn!(target: LOG_TARGET, "the debug UI fixture couldn't open its orderbook route");
                }
            });
        })
        .detach();
    }

    /// Debug UI fixture: a Sell select that lists `sell` alone. The wallet holds none of the
    /// fixture's sell token, so the form would list none.
    #[cfg(debug_assertions)]
    fn ui_fixture_sell_select(
        &self,
        sell: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<SelectState<SearchableVec<PrivateActionAssetSelectItem>>> {
        let sell_item = PrivateActionAssetSelectItem {
            token: sell,
            label: Arc::from(self.token_symbol(sell, cx).as_str()),
            icon_path: self.token_icon(sell, cx),
        };
        cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(vec![sell_item]),
                Some(gpui_component::IndexPath::default()),
                window,
                cx,
            )
        })
    }

    /// Debug UI fixture: open the form of [`Self::open_ui_fixture_retry_review`]. The fixture
    /// holds back the form's own quote and routes, so the synthesized ones stand in for them.
    #[cfg(debug_assertions)]
    fn stage_ui_fixture_retry_review(
        &mut self,
        profile: &wallet_ops::settings::SwapProfile,
        orderbook: CowOrderbookClient,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx) || self.busy() {
            return;
        }
        let Some((record, review, open)) =
            crate::root::private_swap::ui_fixture::retry_review(profile, orderbook.isolation())
        else {
            tracing::warn!(target: LOG_TARGET, "the debug UI fixture couldn't synthesize its retry");
            return;
        };
        // A speed-up mode's form speeds up the synthesized swap's open order.
        let speed_up = record
            .swap()
            .and_then(|swap| swap.orders().iter().find(|order| Some(order.uid()) == open))
            .map(|order| SpeedUpOrder {
                uid: order.uid(),
                bounds: order.bounds().clone(),
            });
        let plan = review.plan();
        // A `non-evm:review` mode's retry delivers to Solana, from 1Click's synthesized list.
        if let (SwapDelivery::NonEvmBridge(delivery), Some((network, tokens))) = (
            plan.delivery(),
            crate::root::private_swap::ui_fixture::non_evm_routes(),
        ) {
            let (sell, amount) = (plan.sell_token(), plan.amount());
            let operation = record.operation();
            self.records = vec![record];
            self.open_form(
                Some(operation),
                sell,
                None,
                Some(amount),
                None,
                SwapDelivery::NonEvmBridge(delivery),
                window,
                cx,
            );
            let sell_select = self.ui_fixture_sell_select(sell, window, cx);
            let Some(form) = self.form.as_mut() else {
                return;
            };
            form.sell_select = sell_select;
            form.bridge
                .non_evm_routes
                .insert((sell, network), Ok(tokens));
            form.set_orderbook(Some(orderbook), None);
            form.quote = QuoteState::Ready(Arc::new(review));
            self.refresh_form_delivery(cx);
            self.sync_gas_controls(window, cx);
            cx.notify();
            return;
        }
        let SwapDelivery::Bridge(delivery) = plan.delivery() else {
            return;
        };
        let (sell, intermediate, amount) = (plan.sell_token(), plan.buy_token(), plan.amount());
        let operation = record.operation();
        self.records = vec![record];
        self.open_form(
            Some(operation),
            sell,
            None,
            Some(amount),
            None,
            SwapDelivery::Bridge(delivery),
            window,
            cx,
        );
        let sell_select = self.ui_fixture_sell_select(sell, window, cx);
        let symbol =
            self.network_token_symbol(delivery.destination_chain, delivery.destination_token, cx);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.sell_select = sell_select;
        form.bridge.routes.insert(
            (sell, delivery.destination_chain),
            Ok(BridgeRoutes {
                across: vec![BridgeDestination {
                    destination_token: delivery.destination_token,
                    intermediate,
                    symbol,
                    same_asset: true,
                    near: None,
                }],
                near: Vec::new(),
                same_asset: Vec::new(),
                across_native: None,
                unavailable: None,
                served: true,
            }),
        );
        form.set_orderbook(Some(orderbook), None);
        form.quote = QuoteState::Ready(Arc::new(review));
        if speed_up.is_some() {
            form.back_to_detail = Some(operation);
        }
        form.speed_up = speed_up;
        // Only a speed-up's earlier approval is left to accept.
        form.price_acknowledged = true;
        form.high_costs_acknowledged = true;
        self.refresh_provider_select(window, cx);
        self.refresh_form_delivery(cx);
        self.sync_gas_controls(window, cx);
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn open_form_with_source(
        &mut self,
        operation: Option<ExecutorOperationId>,
        sell: Address,
        buy: Option<Address>,
        amount: Option<U256>,
        terms: Option<SavedTerms>,
        delivery: SwapDelivery,
        source: Option<PublicAccountMetadata>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        ensure_gas_bar_bindings(cx);
        ensure_buy_picker_bindings(cx);
        if let Some(root) = self.root.upgrade() {
            root.update(cx, |root, _| {
                root.activate_chain_background(self.origin_chain_id);
                if let SwapDelivery::Bridge(delivery) = delivery {
                    root.activate_chain_background(delivery.destination_chain);
                }
            });
        }
        let terms = terms.unwrap_or_default();
        let (receive_to, receiver) = match &delivery {
            // A private Bridge delivery's receiver is its destination stealth account.
            SwapDelivery::Reshield
            | SwapDelivery::Bridge(BridgeDelivery {
                private: Some(_), ..
            }) => (ReceiveTo::PrivateBalance, String::new()),
            SwapDelivery::External { receiver }
            | SwapDelivery::Bridge(BridgeDelivery { receiver, .. }) => {
                (ReceiveTo::PublicAddress, receiver.to_checksum(None))
            }
            SwapDelivery::NonEvmBridge(delivery) => {
                (ReceiveTo::PublicAddress, delivery.receiver.clone())
            }
        };
        let mut bridge = BridgeChoices {
            chosen: None,
            surplus: BridgeSurplus::Reshield,
            shield_failure: BridgeShieldFailure::default(),
            routes: HashMap::new(),
            routes_tasks: HashMap::new(),
            non_evm_routes: HashMap::new(),
            non_evm_tasks: HashMap::new(),
            notice_open: false,
            provider_hint_open: false,
            failure_hint_open: false,
        };
        // A non-EVM delivery restores its network and token, found again in 1Click's list.
        let (non_evm, non_evm_token) = match &delivery {
            SwapDelivery::NonEvmBridge(delivery) => {
                (Some(delivery.network), Some(delivery.contract.clone()))
            }
            _ => (None, None),
        };
        let (network, buy, native_output) = match (delivery, buy) {
            (SwapDelivery::NonEvmBridge(_), _) => (None, None, false),
            (SwapDelivery::Bridge(delivery), _) => {
                // The approved provider stays, even where the other one also delivers.
                bridge.chosen = Some(delivery.provider);
                if delivery.surplus == BridgeSurplus::KeepInAccount {
                    bridge.surplus = BridgeSurplus::KeepInAccount;
                }
                if let Some(private) = delivery.private {
                    bridge.shield_failure = private.on_shield_failure;
                }
                (
                    Some(delivery.destination_chain),
                    Some(self.bridge_received_token(delivery, cx)),
                    false,
                )
            }
            (_, Some(Address::ZERO)) => {
                let wrapped = self.wrapped_native_token(cx);
                (None, wrapped, wrapped.is_some())
            }
            (_, buy) => (None, buy, false),
        };
        let assets = match &source {
            Some(account) => self.public_form_assets(account, sell, cx),
            None => self.form_assets(operation, sell, cx),
        };
        let source_value = source
            .as_ref()
            .map(|account| account.public_account_uuid.clone());
        let pay_from_items = self.swap_source_items(sell, cx);
        let pay_from_select = cx.new(|cx| {
            let mut select =
                SelectState::new(SwapSourceSelect::new(pay_from_items), None, window, cx);
            select.set_selected_value(&source_value, window, cx);
            select
        });
        let sell_items = private_action_asset_select_items(&assets.sell_assets);
        let sell_index = select_index(&sell_items, sell);
        let picker = BuyPicker::new(
            non_evm.map_or_else(
                || BuyNetworkKey::Chain(network.unwrap_or(self.origin_chain_id)),
                BuyNetworkKey::NonEvm,
            ),
            window,
            cx,
        );
        let sell_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(sell_items), sell_index, window, cx)
                .searchable(true)
        });
        // Filled in once the network's routes and the Buy token give a provider.
        let provider_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(Vec::<ProviderSelectItem>::new()),
                None,
                window,
                cx,
            )
        });
        let amount_input = cx.new(|cx| InputState::new(window, cx).placeholder("0.0"));
        let gas_share_bps = terms
            .gas_share_bps
            .unwrap_or(GAS_SHARE_BALANCED_BPS)
            .min(GAS_SHARE_LOOSE_BPS);
        // The knob moves to the quote's position once one is ready.
        let gas_slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(100.)
                .step(f32::from(GAS_BAR_STEP))
                .default_value(f32::from(100 - gas_share_bps / 100))
        });
        let gas_minimum_input = cx.new(|cx| InputState::new(window, cx).placeholder("0.0"));
        let gas_bar_focus = cx.focus_handle().tab_stop(true);
        let receiver_input = new_text_input(window, cx, receiver_placeholder(non_evm));
        if !receiver.is_empty() {
            receiver_input.update(cx, |input, cx| {
                input.set_value(receiver.clone(), window, cx);
            });
        }
        let account_select = (operation.is_none() && source.is_none()).then(|| {
            let items = self.swap_account_items(None, cx);
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(items),
                    Some(gpui_component::IndexPath::default()),
                    window,
                    cx,
                )
                .searchable(true)
            })
        });
        // Its accounts are listed once the form has a destination network.
        let destination_select = operation.is_none().then(|| {
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(vec![new_account_item()]),
                    Some(gpui_component::IndexPath::default()),
                    window,
                    cx,
                )
                .searchable(true)
            })
        });
        let mut subscriptions = vec![
            cx.subscribe_in(
                &pay_from_select,
                window,
                |this, _, event: &SelectEvent<SwapSourceSelect>, window, cx| {
                    if let SelectEvent::Confirm(Some(source)) = event {
                        this.select_form_source(source.as_deref(), window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &amount_input,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change)
                        && this
                            .form
                            .as_ref()
                            .is_some_and(|form| form.amount_input == *input)
                    {
                        this.form_inputs_changed(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &sell_select,
                window,
                |this,
                 _,
                 event: &SelectEvent<SearchableVec<PrivateActionAssetSelectItem>>,
                 window,
                 cx| {
                    if let SelectEvent::Confirm(Some(token)) = event {
                        this.set_form_sell(*token, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &provider_select,
                window,
                |this, _, event: &SelectEvent<SearchableVec<ProviderSelectItem>>, window, cx| {
                    if let SelectEvent::Confirm(Some(provider)) = event {
                        this.set_form_provider(*provider, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &gas_slider,
                window,
                |this, slider, event: &SliderEvent, window, cx| {
                    if this
                        .form
                        .as_ref()
                        .is_none_or(|form| form.gas_slider != *slider)
                    {
                        return;
                    }
                    match event {
                        SliderEvent::Change(value) | SliderEvent::Release(value) => {
                            this.move_gas_bar(value.end(), window, cx);
                        }
                    }
                },
            ),
            cx.subscribe_in(
                &gas_minimum_input,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    if this
                        .form
                        .as_ref()
                        .is_none_or(|form| form.gas_minimum_input != *input)
                    {
                        return;
                    }
                    match event {
                        InputEvent::Change => this.gas_minimum_edited(window, cx),
                        InputEvent::Blur => {
                            // An amount beyond the bar's ends shows the end it was clamped to.
                            this.sync_gas_controls(window, cx);
                        }
                        InputEvent::PressEnter { .. } | InputEvent::Focus => {}
                    }
                },
            ),
            cx.subscribe_in(
                &receiver_input,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    if this
                        .form
                        .as_ref()
                        .is_none_or(|form| form.receiver_input != *input)
                    {
                        return;
                    }
                    match event {
                        InputEvent::Change => this.receiver_edited(window, cx),
                        InputEvent::PressEnter { .. } => {
                            this.confirm_receiver_suggestion(window, cx);
                        }
                        _ => {}
                    }
                },
            ),
        ];
        if let Some(select) = &account_select {
            subscriptions.push(cx.subscribe_in(
                select,
                window,
                |this, _, event: &SelectEvent<SearchableVec<SwapAccountSelectItem>>, window, cx| {
                    if let SelectEvent::Confirm(Some(operation)) = event {
                        this.select_form_account(*operation, window, cx);
                    }
                },
            ));
        }
        if let Some(select) = &destination_select {
            subscriptions.push(cx.subscribe_in(
                select,
                window,
                |this, _, event: &SelectEvent<SearchableVec<SwapAccountSelectItem>>, window, cx| {
                    if let SelectEvent::Confirm(Some(operation)) = event {
                        this.select_form_destination(*operation, window, cx);
                    }
                },
            ));
        }
        let mut form = SwapForm {
            public: source.map(PublicSwapForm::new),
            pay_from_select,
            operation,
            reuse_account: false,
            reuse_use: None,
            account_select,
            destination_account: None,
            destination_select,
            destination_candidates: HashMap::new(),
            sell,
            buy,
            native_output,
            sell_select,
            picker,
            network,
            non_evm,
            non_evm_token,
            provider_select,
            bridge,
            amount_input,
            slippage_bps: terms.slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS),
            gas_share_bps,
            gas_custom: GasPreset::of_share(gas_share_bps).is_none(),
            gas_slider,
            gas_minimum_input,
            gas_minimum_editing: false,
            gas_bar_focus,
            valid_for: terms
                .valid_for
                .unwrap_or_else(|| self.default_valid_for(cx)),
            receive_to,
            receiver_input,
            receiver_value: receiver,
            receiver_picker: RecipientPickerState::default(),
            delivery: Ok(SwapDelivery::Reshield),
            route: SetupRoute::default(),
            destination_route: SetupRoute::default(),
            broadcaster_side: SetupSide::Origin,
            assets,
            quote: QuoteState::Idle,
            quote_task: None,
            quote_revision: 0,
            quote_terms: None,
            bridge_quote_error: None,
            orderbook: None,
            bridge_clients: None,
            quoted_bridge: None,
            price_acknowledged: false,
            high_costs_acknowledged: false,
            continuing_acknowledged: false,
            blocked_shield_acknowledged: false,
            error: None,
            back_to_detail: None,
            speed_up: None,
            details_open: false,
            settings_open: false,
            gas_help_open: false,
            quote_details_open: false,
            _subscriptions: subscriptions,
        };
        if let Some(amount) = amount {
            let decimals = self
                .form_sell_metadata(&form, cx)
                .map(|metadata| metadata.decimals);
            form.amount_input.update(cx, |input, cx| {
                input.set_value(format_unshield_amount_input(amount, decimals), window, cx);
            });
        }
        // A prefilled receiver is checked again, against this form's stealth account.
        form.delivery = self.form_delivery(&form, cx);
        self.form = Some(form);
        self.start_setup_route_updates(window, cx);
        // A restored private Bridge delivery needs its network's session.
        if receive_to == ReceiveTo::PrivateBalance
            && let Some(network) = network
        {
            self.load_private_networks(Some(network), cx);
            self.refresh_destination_accounts(window, cx);
        }
        self.load_bridge_routes(window, cx);
        self.schedule_quote(window, cx);
        self.show_view(SwapDialogView::Form, window, cx);
    }

    pub(super) fn form_mode(&self, form: &SwapForm) -> FormMode {
        if form.public.is_some() {
            return FormMode::Order;
        }
        let record = form.operation.and_then(|operation| self.record(operation));
        // An approved order can use its included setup before the record confirms it.
        // A speed-up reviews a second order while its open one is placed.
        if form.reuse_account
            || form.speed_up.is_some()
            || record.is_some_and(|record| self.approved_use(record).is_some())
        {
            return FormMode::Order;
        }
        swap_form_mode(record.map(|record| self.stage(record)))
    }
}
