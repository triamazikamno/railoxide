use super::*;

impl PrivateSwapsView {
    /// The swap network's bridge parameters.
    pub(super) fn origin_bridge_profile(&self, cx: &App) -> Option<BridgeProfile> {
        self.root
            .upgrade()?
            .read(cx)
            .effective_chain_configs
            .get(self.origin_chain_id)?
            .bridge_profile()
    }

    /// The effective configuration of the form's destination network.
    pub(super) fn destination_chain(
        &self,
        form: &SwapForm,
        cx: &App,
    ) -> Option<EffectiveChainConfig> {
        let network = form.network?;
        self.root
            .upgrade()?
            .read(cx)
            .effective_chain_configs
            .get(network)
            .cloned()
    }

    /// Fetch the routes of the form's network, and of another one the open Buy picker shows,
    /// for the form's sell token.
    pub(super) fn load_bridge_routes(&mut self, window: &Window, cx: &Context<'_, Self>) {
        let own = self.origin_chain_id;
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let shown = form.picker.open.then_some(form.picker.network);
        let shown_chain = shown
            .and_then(BuyNetworkKey::chain_id)
            .filter(|shown| *shown != own && form.network != Some(*shown));
        let shown_non_evm = match shown {
            Some(BuyNetworkKey::NonEvm(network)) if form.non_evm != Some(network) => Some(network),
            _ => None,
        };
        let non_evm = form
            .non_evm
            .into_iter()
            .chain(shown_non_evm)
            .collect::<Vec<_>>();
        for network in form.network.into_iter().chain(shown_chain) {
            self.load_network_routes(network, window, cx);
        }
        for network in non_evm {
            self.load_non_evm_routes(network, window, cx);
        }
    }

    /// Fetch what NEAR Intents delivers on the non-EVM `network` for the form's sell token,
    /// unless it is known or already loading. The network has no chain, so nothing else is
    /// asked of it.
    fn load_non_evm_routes(
        &mut self,
        network: NonEvmNetwork,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        // A Public account's form offers no non-EVM network, and the debug UI fixture's form
        // holds synthesized routes.
        if crate::root::private_swap::ui_fixture::holds_swaps()
            || self.form.as_ref().is_none_or(|form| form.public.is_some())
        {
            return;
        }
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let key = (form.sell, network);
        if form.bridge.non_evm_routes.contains_key(&key)
            || form.bridge.non_evm_tasks.contains_key(&key)
        {
            return;
        }
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let (origin, tokens) = {
            let root = root.read(cx);
            (
                root.effective_chain_configs
                    .get(self.origin_chain_id)
                    .and_then(EffectiveChainConfig::bridge_profile),
                root.effective_token_registry.clone(),
            )
        };
        let Some(origin) = origin else {
            return;
        };
        let tracked = form
            .operation
            .and_then(|operation| self.tracking.get(&operation))
            .and_then(|tracking| tracking.orderbook.clone());
        let orderbook = form.orderbook.clone().or(tracked);
        let bridge_clients = form
            .orderbook
            .as_ref()
            .and_then(|_| form.bridge_clients.clone());
        let operation = form.operation;
        let (Some(owner), Some(http)) = (self.private_owner().cloned(), self.http(cx)) else {
            return;
        };
        let runtime = self.runtime.clone();
        let task = cx.spawn_in(window, async move |view, cx| {
            let result = runtime
                .spawn(request_non_evm_routes(
                    owner,
                    http,
                    orderbook,
                    bridge_clients,
                    origin,
                    network,
                    key.0,
                    tokens,
                ))
                .await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.apply_non_evm_routes(operation, key, result.ok(), window, cx);
            });
        });
        if let Some(form) = self.form.as_mut() {
            form.bridge.non_evm_tasks.insert(key, task);
        }
    }

    fn apply_non_evm_routes(
        &mut self,
        operation: Option<ExecutorOperationId>,
        key: (Address, NonEvmNetwork),
        result: Option<BridgeRoutesResult<Vec<NonEvmBridgeDestination>>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.operation != operation || form.bridge.non_evm_tasks.remove(&key).is_none() {
            return;
        }
        let routes = self.adopt_bridge_routes_result(operation, result);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.bridge.non_evm_routes.insert(key, routes);
        // Routes only the Buy picker shows leave the form's delivery and its quote as they are.
        if (form.sell, form.non_evm) != (key.0, Some(key.1)) {
            cx.notify();
            return;
        }
        form.bridge.provider_hint_open = false;
        self.bridge_choices_changed(window, cx);
    }

    /// Fetch the routes of `network` for the form's sell token, unless they are known or
    /// already loading. A missing entry reads as loading.
    fn load_network_routes(&mut self, network: u64, window: &Window, cx: &Context<'_, Self>) {
        if self.form.as_ref().is_some_and(|form| form.public.is_some()) {
            self.load_public_network_routes(network, window, cx);
            return;
        }
        // Debug UI fixture: its form's routes are synthesized, and no provider is asked.
        if crate::root::private_swap::ui_fixture::holds_swaps() {
            return;
        }
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let key = (form.sell, network);
        if form.bridge.routes.contains_key(&key) || form.bridge.routes_tasks.contains_key(&key) {
            return;
        }
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let (origin, destination, tokens, across_native) = {
            let root = root.read(cx);
            (
                root.effective_chain_configs
                    .get(self.origin_chain_id)
                    .and_then(EffectiveChainConfig::bridge_profile),
                root.effective_chain_configs
                    .get(network)
                    .and_then(EffectiveChainConfig::bridge_destination),
                root.effective_token_registry.clone(),
                root.effective_chain_configs
                    .get(network)
                    .and_then(across_unwrapped_token),
            )
        };
        let (Some(origin), Some(destination)) = (origin, destination) else {
            return;
        };
        let tracked = form
            .operation
            .and_then(|operation| self.tracking.get(&operation))
            .and_then(|tracking| tracking.orderbook.clone());
        let orderbook = form.orderbook.clone().or(tracked);
        let bridge_clients = form
            .orderbook
            .as_ref()
            .and_then(|_| form.bridge_clients.clone());
        let operation = form.operation;
        let (Some(owner), Some(http)) = (self.private_owner().cloned(), self.http(cx)) else {
            return;
        };
        let runtime = self.runtime.clone();
        let task = cx.spawn_in(window, async move |view, cx| {
            let result = runtime
                .spawn(request_bridge_routes(
                    owner,
                    http,
                    orderbook,
                    bridge_clients,
                    origin,
                    destination,
                    key.0,
                    tokens,
                    across_native,
                ))
                .await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.apply_bridge_routes(operation, key, result.ok(), window, cx);
            });
        });
        if let Some(form) = self.form.as_mut() {
            form.bridge.routes_tasks.insert(key, task);
        }
    }

    fn apply_bridge_routes(
        &mut self,
        operation: Option<ExecutorOperationId>,
        key: (Address, u64),
        result: Option<BridgeRoutesResult>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.operation != operation || form.bridge.routes_tasks.remove(&key).is_none() {
            return;
        }
        let routes = self.adopt_bridge_routes_result(operation, result);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.bridge.routes.insert(key, routes);
        // Routes only the Buy picker shows leave the form's delivery and its quote as they are.
        if (form.sell, form.network) != (key.0, Some(key.1)) {
            cx.notify();
            return;
        }
        form.bridge.notice_open = false;
        form.bridge.provider_hint_open = false;
        self.bridge_choices_changed(window, cx);
    }

    /// The routes of a finished route load, adopting the orderbook route it created into the
    /// form and the operation's tracking. A load that ended without a result reads as an error.
    fn adopt_bridge_routes_result<R>(
        &mut self,
        operation: Option<ExecutorOperationId>,
        result: Option<BridgeRoutesResult<R>>,
    ) -> eyre::Result<R> {
        let Some(result) = result else {
            return Err(eyre::eyre!(
                "Loading the bridge routes stopped unexpectedly. Try again."
            ));
        };
        // The form keeps its own route when it got one meanwhile.
        if let Some(form) = self.form.as_mut()
            && form.orderbook.is_none()
            && let Some(orderbook) = result.orderbook
        {
            form.set_orderbook(Some(orderbook.clone()), result.bridge_clients);
            if let Some(operation) = operation {
                self.tracking
                    .entry(operation)
                    .or_default()
                    .set_route(Some(orderbook));
            }
        }
        result.routes
    }

    /// Ask the providers again on a fresh route, as quote retries do: for every network whose
    /// routes failed or lack a provider.
    pub(super) fn retry_bridge_routes(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.quote_details_open = false;
        if let Some(public) = form.public.as_mut() {
            public.invalidate_quote_route();
            form.quote_revision = form.quote_revision.wrapping_add(1);
            form.quote_task = None;
            form.quote = QuoteState::Idle;
            form.bridge_quote_error = None;
            form.price_acknowledged = false;
            form.high_costs_acknowledged = false;
            form.continuing_acknowledged = false;
            public.route_errors.clear();
            public.route_tasks.clear();
            self.load_bridge_routes(window, cx);
            cx.notify();
            return;
        }
        form.bridge
            .routes
            .retain(|_, routes| matches!(routes, Ok(routes) if routes.unavailable.is_none()));
        form.bridge.routes_tasks.clear();
        form.bridge
            .non_evm_routes
            .retain(|_, routes| routes.is_ok());
        form.bridge.non_evm_tasks.clear();
        form.bridge.notice_open = false;
        form.bridge.provider_hint_open = false;
        form.set_orderbook(None, None);
        if let Some(tracking) = form
            .operation
            .and_then(|operation| self.tracking.get_mut(&operation))
        {
            tracking.set_route(None);
        }
        self.load_bridge_routes(window, cx);
        cx.notify();
    }

    /// The network, Buy token, provider, routes or sell token changed: rebuild the Provider
    /// list, check the delivery again and quote it, which clears acknowledgements.
    pub(super) fn bridge_choices_changed(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.form.as_ref().is_some_and(|form| form.public.is_some()) {
            self.refresh_account_select(window, cx);
            self.refresh_setup_route(cx);
            self.refresh_form_delivery(cx);
            self.schedule_quote(window, cx);
            cx.notify();
            return;
        }
        self.refresh_provider_select(window, cx);
        self.refresh_account_select(window, cx);
        // A private Bridge delivery's destination account has a setup route of its own.
        self.refresh_setup_route(cx);
        self.delivery_changed(window, cx);
    }

    /// Follow the delivery with the account selects: a private Bridge swap has one for each
    /// network, and its destination select lists the form's network's accounts. The swap's own
    /// selected account stays, and so does a destination chosen on the same network.
    fn refresh_account_select(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.refresh_destination_accounts(window, cx);
        // The select follows a choice the network change cleared, or one that is back in
        // effect, also while the listed accounts are the same.
        self.refresh_destination_select(window, cx);
    }

    /// List both providers, each unavailable one with its reason, and select the one the Buy
    /// token uses. Without one, the select shows why.
    pub(super) fn refresh_provider_select(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let (items, selected) = match form.bridge_state() {
            BridgeState::Ready {
                destination,
                provider,
                across,
                near,
                ..
            } => {
                let network = network_name(form.network.unwrap_or(self.origin_chain_id));
                let unreachable = form.bridge_unavailable();
                let across_reason = (!across).then(|| {
                    let reason = if unreachable == Some(BridgeProvider::Across) {
                        "Unreachable now".to_owned()
                    } else if destination.destination_token == Address::ZERO {
                        format!("Doesn't deliver {}", destination.symbol)
                    } else {
                        format!("No route to {network}")
                    };
                    reason.into()
                });
                let near_reason = (!near).then(|| {
                    if unreachable == Some(BridgeProvider::NearIntents) {
                        "Unreachable now".into()
                    } else {
                        format!("Not listed on {network}").into()
                    }
                });
                (
                    vec![
                        ProviderSelectItem {
                            provider: BridgeProvider::Across,
                            unavailable: across_reason,
                        },
                        ProviderSelectItem {
                            provider: BridgeProvider::NearIntents,
                            unavailable: near_reason,
                        },
                    ],
                    Some(provider),
                )
            }
            _ => (Vec::new(), None),
        };
        form.provider_select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(items), window, cx);
            if let Some(provider) = selected {
                select.set_selected_value(&provider, window, cx);
            } else {
                select.set_selected_index(None, window, cx);
            }
        });
    }
}
