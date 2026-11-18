use super::*;

impl PrivateSwapsView {
    pub(in crate::root::private_swap::form) fn schedule_public_quote(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.load_public_orders_available(window, cx);
        // Route-list replies must not replace a signing-time quote with another optimistic
        // preview. An ordinary draft edit still invalidates it through the same terms check.
        let retain_delivery = self.form.as_ref().is_some_and(|form| {
            form.public.as_ref().is_some_and(|public| {
                public.delivery_review
                    && self
                        .public_origin(cx)
                        .zip(self.form_amount(form, cx).ok())
                        .is_some_and(|(origin, amount)| {
                            public
                                .quote_terms
                                .as_ref()
                                .is_some_and(|terms| terms.matches(form, &origin, amount))
                        })
            })
        });
        if retain_delivery {
            return;
        }
        // A changed draft loses its old review even when the new route or destination is
        // unavailable. An unquotable pair must never retain approval of a different pair.
        let revision = {
            let Some(form) = self.form.as_mut() else {
                return;
            };
            let Some(public) = form.public.as_mut() else {
                return;
            };
            form.quote_revision = form.quote_revision.wrapping_add(1);
            form.quote_task = None;
            form.price_acknowledged = false;
            form.high_costs_acknowledged = false;
            form.quote = QuoteState::Idle;
            form.quote_details_open = false;
            form.bridge_quote_error = None;
            public.review = None;
            public.quote_terms = None;
            public.delivery_review = false;
            form.quote_revision
        };
        // The draft's pair may have changed, and with it the open swap that blocks it.
        self.refresh_public_conflict(cx);
        cx.notify();
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(public) = form.public.as_ref() else {
            return;
        };
        let amount = self.form_amount(form, cx);
        let Some(network) = form.network else {
            return;
        };
        let Some(destination) = form
            .buy
            .and_then(|buy| {
                public
                    .routes
                    .get(&(form.sell, network))?
                    .iter()
                    .find(|route| route.destination.destination_token == buy)
            })
            .cloned()
        else {
            return;
        };
        let Some(owner) = self.public_quote_destination(network, cx) else {
            return;
        };
        let Some(origin) = self.public_origin(cx) else {
            return;
        };
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let (http, registry, anchors) = {
            let root = root.read(cx);
            (
                root.http.clone(),
                root.effective_token_registry.clone(),
                root.public_broadcaster_anchor_cache.clone(),
            )
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(public) = form.public.as_mut() else {
            return;
        };
        public.review = None;
        let Ok(amount) = amount else {
            form.quote = QuoteState::Idle;
            form.quote_details_open = false;
            cx.notify();
            return;
        };
        let (source, sell, slippage, gas_share, failure) = (
            public.account.address,
            form.sell,
            form.slippage_bps,
            form.gas_share_bps,
            form.bridge.shield_failure,
        );
        let clients = public.clients.clone();
        let quoted = PublicQuoteTerms {
            source: public.account.clone(),
            origin: origin.clone(),
            network,
            sell,
            buy: destination.destination.destination_token,
            amount,
            slippage_bps: slippage,
            gas_share_bps: gas_share,
            shield_failure: failure,
        };
        form.quote = QuoteState::Loading;
        form.quote_details_open = false;
        let runtime = self.runtime.clone();
        form.quote_task = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor().timer(QUOTE_DEBOUNCE).await;
            let task = runtime.spawn(async move {
                let clients = match clients {
                    Some(clients) => clients,
                    None => public_swap_clients(&origin, &http).await?,
                };
                let fee = quote_public_action_gas_fee(origin.chain_id, &origin, &http).await?;
                let review = owner
                    .review_public_swap(PublicSwapReviewRequest {
                        origin: &origin,
                        source,
                        sell: public_sell(&origin, sell)?,
                        sell_amount: amount,
                        destination: &destination,
                        slippage_bps: slippage,
                        gas_share_bps: gas_share,
                        on_shield_failure: failure,
                        orderbook: clients.orderbook.as_ref(),
                        across: &clients.across,
                        anchor_cache: Some(&anchors),
                        token_registry: &registry,
                        // A deposit-only network has no profile: 300 is every built-in
                        // profile's limit, as the form's own fallback assumes.
                        anchor_deviation_bps: origin
                            .public_swap_profile()
                            .map_or(300, |profile| profile.anchor_deviation_bps()),
                        max_fee_per_gas: fee.suggested_max_fee_per_gas,
                        max_priority_fee_per_gas: fee.suggested_max_priority_fee_per_gas,
                        client: &clients.http,
                    })
                    .await?;
                Ok::<_, eyre::Report>((clients, review, quoted))
            });
            let _abort = AbortOnDrop(task.abort_handle());
            let result = task.await;
            let _ = view.update(cx, |view, cx| {
                let Some(form) = view
                    .form
                    .as_mut()
                    .filter(|form| form.quote_revision == revision)
                else {
                    return;
                };
                let Some(public) = form
                    .public
                    .as_mut()
                    .filter(|public| public.account.address == source)
                else {
                    return;
                };
                match result {
                    Ok(Ok((clients, review, terms))) => {
                        if public.account != terms.source {
                            return;
                        }
                        public.clients = Some(clients);
                        public.review = Some(Arc::new(review));
                        public.quote_terms = Some(terms);
                        form.quote = QuoteState::Idle;
                        form.quote_details_open = false;
                        form.error = None;
                    }
                    Ok(Err(error)) => form.quote = QuoteState::Failed(error),
                    Err(_) => {
                        form.quote =
                            QuoteState::Failed(eyre::eyre!("The quote stopped. Try again."));
                    }
                }
                form.quote_task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The form's gas share changed. A reviewed order is priced at it locally and its bridge
    /// leg previewed again, without asking the orderbook. Before a review, the share prices
    /// the next quote.
    pub(in crate::root::private_swap::form) fn public_gas_share_changed(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let reviewed = self
            .form
            .as_ref()
            .and_then(|form| form.public.as_ref())
            .is_some_and(|public| public.review.is_some());
        if reviewed {
            self.schedule_public_gas_share(window, cx);
        } else {
            self.schedule_public_quote(window, cx);
        }
    }

    /// Preview the bridge leg again for the share the gas strip shows, after the quote
    /// debounce. The review stays in view, and the orderbook isn't asked. A share back at the
    /// reviewed one has nothing to refresh, which also ends a refresh still under way.
    fn schedule_public_gas_share(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(public) = form.public.as_ref() else {
            return;
        };
        let (Some(review), Some(strip)) = (public.review.clone(), public_strip(form)) else {
            return;
        };
        let gas_share_bps = strip.share_bps;
        let pending = strip.pending();
        let clients = public.clients.clone();
        let owner = form
            .network
            .and_then(|network| self.public_quote_destination(network, cx));
        let registries = self.root.upgrade().map(|root| {
            let root = root.read(cx);
            (
                root.public_broadcaster_anchor_cache.clone(),
                root.effective_token_registry.clone(),
            )
        });
        let runtime = self.runtime.clone();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        // A reviewed order's task is an earlier refresh. Dropping it stops its request.
        form.quote_task = None;
        form.quote_revision = form.quote_revision.wrapping_add(1);
        form.bridge_quote_error = None;
        if !pending {
            cx.notify();
            return;
        }
        form.price_acknowledged = false;
        form.high_costs_acknowledged = false;
        if let Some(public) = form.public.as_mut() {
            public.delivery_review = false;
        }
        let (Some(clients), Some(owner), Some((anchors, registry))) = (clients, owner, registries)
        else {
            form.bridge_quote_error =
                Some("The swap's bridge route isn't ready. Retry the quote.".into());
            cx.notify();
            return;
        };
        let revision = form.quote_revision;
        form.quote_task = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor().timer(QUOTE_DEBOUNCE).await;
            let quoted = Arc::clone(&review);
            let work = runtime.spawn(async move {
                Box::pin(owner.requote_public_swap_gas_share(
                    &quoted,
                    gas_share_bps,
                    &clients.across,
                    Some(&anchors),
                    &registry,
                ))
                .await
            });
            // Dropping a Tokio handle detaches its task, so a superseded refresh aborts it.
            let _abort = AbortOnDrop(work.abort_handle());
            let result = work.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.apply_public_gas_share(
                    revision,
                    &review,
                    gas_share_bps,
                    result.ok(),
                    window,
                    cx,
                );
            });
        }));
        cx.notify();
    }

    /// Install a gas share refresh's reply, if the form still shows the review and the share
    /// it was asked for. A failure keeps the review and the share, and says why.
    fn apply_public_gas_share(
        &mut self,
        revision: u64,
        quoted: &Arc<PublicSwapReview>,
        gas_share_bps: u16,
        result: Option<eyre::Result<PublicSwapReview>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self
            .form
            .as_mut()
            .filter(|form| form.quote_revision == revision)
        else {
            return;
        };
        form.quote_task = None;
        let Some(public) = form.public.as_mut().filter(|public| {
            public
                .review
                .as_ref()
                .is_some_and(|review| Arc::ptr_eq(review, quoted))
        }) else {
            cx.notify();
            return;
        };
        match result {
            Some(Ok(review)) => {
                // The destination terms changed, so consent to the old ones doesn't carry over.
                form.price_acknowledged = false;
                form.high_costs_acknowledged = false;
                if let Some(terms) = public.quote_terms.as_mut() {
                    terms.gas_share_bps = gas_share_bps;
                }
                public.review = Some(Arc::new(review));
            }
            Some(Err(error)) => {
                form.bridge_quote_error = Some(
                    bridge_unreachable(&error)
                        .unwrap_or_else(|| format!("{error:#}"))
                        .into(),
                );
            }
            None => {
                form.bridge_quote_error =
                    Some("Updating the bridge quote stopped unexpectedly. Try again.".into());
            }
        }
        self.sync_gas_controls(window, cx);
        cx.notify();
    }

    /// The open swap of the form's Public account that already trades one of the draft's
    /// tokens, as the destination network's owner reads it from the wallet's records. Known
    /// once the form has a source, a Sell token and a routed Buy token, before Review.
    fn public_source_conflict(&self, cx: &App) -> Option<PublicSwapConflict> {
        let form = self.form.as_ref()?;
        let public = form.public.as_ref()?;
        let route = public_route(form)?;
        let (_, owner) = self.destination_owner(form.network?, cx)?;
        let bridged = route.destination.intermediate;
        let refusal = owner
            .public_swap_source_conflict(&PublicSwapSourceTerms {
                // A swap the form continues doesn't compete with itself.
                id: public.swap_use,
                origin_chain: self.origin_chain_id,
                source: public.account.address,
                source_scope: public.account.scope.clone(),
                sell_token: form.sell,
                bridged_token: bridged,
                order: route.path == PublicBridgePath::Order,
                now: now_unix(),
            })
            .ok()??;
        PublicSwapConflict::naming(&refusal, form, form.sell, bridged)
    }

    /// Read again which open swap, if any, blocks the draft's pair.
    pub(super) fn refresh_public_conflict(&mut self, cx: &App) {
        let conflict = self.public_source_conflict(cx);
        if let Some(public) = self.form.as_mut().and_then(|form| form.public.as_mut()) {
            public.conflict = conflict;
        }
    }

    /// The owner on `network` that a quote for a delivery there reads. A network that was
    /// ready and syncs again still quotes. Reviewing a swap needs it ready.
    fn public_quote_destination(
        &self,
        network: u64,
        cx: &mut Context<'_, Self>,
    ) -> Option<Arc<ExecutorOwner>> {
        if self.public_synced.contains(&network)
            && let Some((_, owner)) = self.destination_owner(network, cx)
        {
            return Some(owner);
        }
        self.ready_destination(network, cx)
            .ok()
            .map(|(_, owner)| owner)
    }
}
