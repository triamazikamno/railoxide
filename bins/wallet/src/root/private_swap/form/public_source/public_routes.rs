use super::*;

impl PrivateSwapsView {
    pub(super) fn public_origin(&self, cx: &App) -> Option<EffectiveChainConfig> {
        self.root
            .upgrade()?
            .read(cx)
            .effective_chain_configs
            .get(self.origin_chain_id)
            .cloned()
    }

    pub(in crate::root::private_swap::form) fn public_form_delivery(
        &self,
        form: &SwapForm,
        cx: &App,
    ) -> Result<SwapDelivery, DeliveryProblem> {
        // An untouched form has no network yet, which the Buy card's label prompts for. Only
        // a picked network that can't be used is a problem.
        let network = form.network.ok_or(DeliveryProblem::Bridge)?;
        if network == self.origin_chain_id {
            return Err(DeliveryProblem::Network {
                problem: "Choose another network for the private balance.".into(),
                syncing: false,
            });
        }
        let root = self.root.upgrade().ok_or(DeliveryProblem::Bridge)?;
        let root = root.read(cx);
        let chain = root
            .effective_chain_configs
            .get(network)
            .ok_or(DeliveryProblem::Bridge)?;
        if chain.bridge_profile().is_none() || chain.swap_profile().is_none() {
            return Err(DeliveryProblem::Network {
                problem: "Private bridge delivery isn't available on this network.".into(),
                syncing: false,
            });
        }
        let token = form
            .buy
            .filter(|token| *token != Address::ZERO)
            .ok_or(DeliveryProblem::Bridge)?;
        let public = form.public.as_ref().ok_or(DeliveryProblem::Bridge)?;
        if !public
            .routes
            .get(&(form.sell, network))
            .is_some_and(|routes| {
                routes
                    .iter()
                    .any(|route| route.destination.destination_token == token)
            })
        {
            return Err(DeliveryProblem::Bridge);
        }
        Ok(SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: network,
            destination_token: token,
            receiver: form
                .destination_choice()
                .map_or(Address::ZERO, |account| account.address),
            surplus: BridgeSurplus::Reshield,
            private: Some(BridgePrivateDelivery {
                on_shield_failure: form.bridge.shield_failure,
            }),
        }))
    }

    /// Whether the form's Public account can place orders: its chain has a Public swap profile
    /// and was read to take orders.
    pub(super) const fn public_can_swap(
        origin: &EffectiveChainConfig,
        public: &PublicSwapForm,
    ) -> bool {
        origin.public_swap_profile().is_some() && public.takes_orders()
    }

    /// List the form's routes from what Across answered: with the tokens an order reaches only
    /// where the account's chain takes orders, otherwise what a deposit serves.
    pub(super) fn list_public_routes(&mut self, cx: &App) {
        let Some(origin) = self.public_origin(cx) else {
            return;
        };
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let Some(public) = self.form.as_mut().and_then(|form| form.public.as_mut()) else {
            return;
        };
        let can_swap = Self::public_can_swap(&origin, public);
        let registry = &root.read(cx).effective_token_registry;
        for (key, (sell, routes)) in &public.across_routes {
            public.routes.insert(
                *key,
                public_across_destination_tokens(
                    routes,
                    *sell,
                    can_swap,
                    registry,
                    origin.chain_id,
                    key.1,
                ),
            );
        }
    }

    /// Read whether the account's chain takes orders, once for this form; a reopened form reads
    /// it again. The read is of the account's own chain and binds no destination, so it goes
    /// through the owner of the form's network when that network is ready, otherwise through
    /// any ready network's. Until one is ready nothing is read.
    pub(super) fn load_public_orders_available(&mut self, window: &Window, cx: &Context<'_, Self>) {
        let Some(origin) = self.public_origin(cx) else {
            return;
        };
        if origin.public_swap_profile().is_none() {
            return;
        }
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(public) = form.public.as_ref() else {
            return;
        };
        if public.orders_available.is_some() || public.orders_task.is_some() {
            return;
        }
        // The read is of public chain state, so a network still syncing serves it too.
        let ready = |state: &ChainUtxoState| match state {
            ChainUtxoState::Syncing { session, .. } | ChainUtxoState::Ready { session, .. } => {
                session.executor_owner()
            }
            _ => None,
        };
        let states = &root.read(cx).chain_states;
        let Some(owner) = form
            .network
            .and_then(|network| states.get(&network))
            .and_then(ready)
            .or_else(|| states.values().find_map(ready))
        else {
            return;
        };
        // The form's route, or one of the read's own before the form has one.
        let client = public.clients.as_ref().map(|clients| clients.http.clone());
        let http = root.read(cx).http.clone();
        let runtime = self.runtime.clone();
        let task = cx.spawn_in(window, async move |view, cx| {
            let task = runtime.spawn(async move {
                let client = match client {
                    Some(client) => client,
                    None => http.operation_http_client().await?,
                };
                owner.public_swap_orders_available(&origin, &client).await
            });
            let _abort = AbortOnDrop(task.abort_handle());
            // A chain that couldn't be read stays unread, so the next quote reads it again.
            let available = match task.await {
                Ok(Ok(available)) => Some(available),
                _ => None,
            };
            let _ = view.update_in(cx, |view, window, cx| {
                let Some(public) = view.form.as_mut().and_then(|form| form.public.as_mut()) else {
                    return;
                };
                public.orders_task = None;
                public.orders_available = available;
                let reviewed = public.review.is_some();
                if available == Some(true) {
                    // The routes read so far listed only what a deposit serves.
                    view.list_public_routes(cx);
                    view.refresh_form_delivery(cx);
                    if !reviewed {
                        view.schedule_public_quote(window, cx);
                    }
                }
                cx.notify();
            });
        });
        if let Some(public) = self.form.as_mut().and_then(|form| form.public.as_mut()) {
            public.orders_task = Some(task);
        }
    }

    pub(in crate::root::private_swap::form) fn load_public_network_routes(
        &mut self,
        network: u64,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        let Some(origin) = self.public_origin(cx) else {
            return;
        };
        self.load_public_orders_available(window, cx);
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let http = root.read(cx).http.clone();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let key = (form.sell, network);
        let Some(public) = form.public.as_mut() else {
            return;
        };
        if network == origin.chain_id
            || public.routes.contains_key(&key)
            || public.route_tasks.contains_key(&key)
        {
            return;
        }
        public.route_errors.remove(&key);
        let runtime = self.runtime.clone();
        public.route_tasks.insert(
            key,
            cx.spawn_in(window, async move |view, cx| {
                let task = runtime.spawn(async move {
                    let clients = public_swap_clients(&origin, &http).await?;
                    let routes = clients
                        .across
                        .available_routes(origin.chain_id, network)
                        .await?;
                    Ok::<_, eyre::Report>((clients, public_sell(&origin, key.0)?, routes))
                });
                let _abort = AbortOnDrop(task.abort_handle());
                let result = task.await;
                let _ = view.update_in(cx, |view, window, cx| {
                    let Some(form) = view.form.as_mut() else {
                        return;
                    };
                    let Some(public) = form.public.as_mut() else {
                        return;
                    };
                    public.route_tasks.remove(&key);
                    match result {
                        Ok(Ok((clients, sell, routes))) => {
                            if !public.delivery_review {
                                public.clients = Some(clients);
                            }
                            public.across_routes.insert(key, (sell, routes));
                            view.list_public_routes(cx);
                        }
                        Ok(Err(error)) => {
                            public.route_errors.insert(key, format!("{error:#}"));
                        }
                        Err(_) => {
                            public
                                .route_errors
                                .insert(key, "The route request stopped. Try again.".into());
                        }
                    }
                    view.refresh_form_delivery(cx);
                    view.schedule_public_quote(window, cx);
                    cx.notify();
                });
            }),
        );
    }
}
