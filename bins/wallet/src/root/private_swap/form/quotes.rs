use super::*;

impl PrivateSwapsView {
    /// What a quote of the form as it stands is requested for, once it has a Buy token and an
    /// amount.
    pub(super) fn form_quote_terms(&self, form: &SwapForm, cx: &App) -> Option<QuoteTerms> {
        // Before setup the quote uses a stand-in. Set-up accounts use local observations;
        // execution preparation refreshes the account before proving and signing.
        let executor = match (self.form_mode(form), form.operation) {
            (FormMode::Setup { resume: true }, Some(operation)) => QuoteExecutor::Setup(operation),
            (FormMode::Order, Some(operation)) => match &form.speed_up {
                Some(open) => QuoteExecutor::SpeedUp {
                    operation,
                    order: open.uid,
                },
                None => QuoteExecutor::Order {
                    operation,
                    reuse: form.reuse_account,
                },
            },
            _ => QuoteExecutor::Preview,
        };
        let (buy, amount) = form.quote_buy().zip(self.form_amount(form, cx).ok())?;
        Some(QuoteTerms {
            executor,
            sell: form.sell,
            buy,
            amount,
            slippage_bps: form.slippage_bps,
            receive_to: form.receive_to,
            bridge: form.bridge_terms(),
        })
    }

    pub(super) fn schedule_quote(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if self.form.as_ref().is_some_and(|form| form.public.is_some()) {
            self.schedule_public_quote(window, cx);
            return;
        }
        // Debug UI fixture: its form holds a synthesized quote, which no request replaces.
        if crate::root::private_swap::ui_fixture::holds_swaps() {
            return;
        }
        if let Some(form) = self.form.as_mut() {
            form.price_acknowledged = false;
            form.high_costs_acknowledged = false;
            form.continuing_acknowledged = false;
            // A ready quote's task is its bridge refresh, which a full quote supersedes.
            if matches!(form.quote, QuoteState::Ready(_)) {
                form.quote_task = None;
                form.quote_revision = form.quote_revision.wrapping_add(1);
            }
            form.bridge_quote_error = None;
        }
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let operation = form.operation;
        let mode = self.form_mode(form);
        if !matches!(mode, FormMode::Setup { .. } | FormMode::Order) {
            return;
        }
        let (gas_share_bps, valid_for) = (form.gas_share_bps, form.valid_for);
        let registries = self.root.upgrade().map(|root| {
            let root = root.read(cx);
            (
                Arc::clone(&root.public_broadcaster_anchor_cache),
                root.effective_token_registry.clone(),
            )
        });
        let bridge = match form.bridge_state() {
            BridgeState::Ready { destination, .. } => self
                .destination_chain(form, cx)
                .zip(self.origin_bridge_profile(cx))
                .map(|(chain, origin)| QuoteBridgeRequest {
                    destination: QuoteBridgeDestination::Chain {
                        destination: Some(destination.clone()),
                        chain,
                    },
                    origin,
                }),
            BridgeState::NonEvm(destination) => {
                self.origin_bridge_profile(cx)
                    .map(|origin| QuoteBridgeRequest {
                        destination: QuoteBridgeDestination::NonEvm(Some(destination.clone())),
                        origin,
                    })
            }
            _ => None,
        };
        let (Some(terms), Some((anchor_cache, tokens)), Some(delivery)) = (
            self.form_quote_terms(form, cx),
            registries,
            form.quote_delivery(),
        ) else {
            if let Some(form) = self.form.as_mut() {
                form.quote = QuoteState::Idle;
                form.quote_task = None;
                form.quote_terms = None;
                form.gas_help_open = false;
                form.quote_details_open = false;
            }
            cx.notify();
            return;
        };
        let inclusion = operation
            .and_then(|operation| self.record(operation))
            .and_then(|record| self.setup_inclusion(record));
        let tracking = operation.map(|operation| self.tracking.entry(operation).or_default());
        let byte_budget = tracking.as_ref().and_then(|tracking| tracking.byte_budget);
        let tracked_orderbook = tracking.and_then(|tracking| {
            tracking.amount = Some(terms.amount);
            tracking.slippage_bps = Some(terms.slippage_bps);
            tracking.gas_share_bps = Some(gas_share_bps);
            tracking.valid_for = Some(valid_for);
            tracking.orderbook.clone()
        });
        let (Some(owner), Some(session), Some(http)) = (
            self.private_owner().cloned(),
            self.private_session().cloned(),
            self.http(cx),
        ) else {
            return;
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let request = QuoteRequest {
            http,
            executor: terms.executor,
            inclusion,
            sell: terms.sell,
            buy: terms.buy,
            amount: terms.amount,
            delivery,
            slippage_bps: terms.slippage_bps,
            gas_share_bps,
            valid_for,
            byte_budget,
            orderbook: form.orderbook.clone().or(tracked_orderbook),
            bridge_clients: form
                .orderbook
                .as_ref()
                .and_then(|_| form.bridge_clients.clone()),
            bridge,
            // A fresh signing-time failure must reach the acknowledgement even if the
            // background cache still contains an older rate.
            anchor_cache: if operation.is_some_and(|operation| {
                self.reapproval == Some((operation, SwapReviewChange::PriceUnavailable))
            }) {
                None
            } else {
                Some(anchor_cache)
            },
            tokens,
        };
        let runtime = self.runtime.clone();
        form.quote_revision = form.quote_revision.wrapping_add(1);
        let revision = form.quote_revision;
        form.quote_terms = Some(terms);
        form.quote = QuoteState::Loading;
        form.gas_help_open = false;
        form.quote_details_open = false;
        form.quote_task = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor().timer(QUOTE_DEBOUNCE).await;
            let result = runtime.spawn(quote_swap(owner, session, request)).await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.apply_quote(operation, revision, result.ok(), window, cx);
            });
        }));
        cx.notify();
    }

    /// Quote a Bridge swap's bridge leg again at the share its strip previews, after the
    /// debounce. The ready quote stays in view, and the orderbook isn't asked. A share back at
    /// the quoted one has nothing to refresh, which also ends a refresh still under way.
    pub(super) fn schedule_bridge_quote(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        // Debug UI fixture: its synthesized quote has no bridge to ask again.
        if crate::root::private_swap::ui_fixture::holds_swaps() {
            return;
        }
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let QuoteState::Ready(review) = &form.quote else {
            return;
        };
        let review = Arc::clone(review);
        let operation = form.operation;
        let gas_share_bps = strip_review(form, &review).gas_share_bps();
        let pending = gas_share_bps != review.gas_share_bps()
            && matches!(
                self.form_mode(form),
                FormMode::Setup { .. } | FormMode::Order
            );
        let bridge = form.quoted_bridge.clone();
        // As a full quote, a refresh after a signing-time price failure skips the anchor cache.
        let price_unavailable = operation.is_some_and(|operation| {
            self.reapproval == Some((operation, SwapReviewChange::PriceUnavailable))
        });
        let registries = self.root.upgrade().map(|root| {
            let root = root.read(cx);
            (
                (!price_unavailable).then(|| Arc::clone(&root.public_broadcaster_anchor_cache)),
                root.effective_token_registry.clone(),
            )
        });
        let Some(owner) = self.private_owner().cloned() else {
            return;
        };
        let runtime = self.runtime.clone();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        // A ready quote's task is an earlier refresh. Dropping it stops its request.
        form.quote_task = None;
        form.quote_revision = form.quote_revision.wrapping_add(1);
        form.bridge_quote_error = None;
        if !pending {
            cx.notify();
            return;
        }
        form.price_acknowledged = false;
        form.high_costs_acknowledged = false;
        form.continuing_acknowledged = false;
        let (Some(bridge), Some((anchor_cache, tokens))) = (bridge, registries) else {
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
                Box::pin(owner.requote_swap_bridge(
                    &quoted,
                    gas_share_bps,
                    bridge.route(),
                    anchor_cache.as_deref(),
                    &tokens,
                ))
                .await
            });
            // Dropping a Tokio handle detaches its task, so a superseded refresh aborts it.
            let _abort = AbortOnDrop(work.abort_handle());
            let result = work.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.apply_bridge_quote(
                    operation,
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

    /// Install a bridge refresh's reply, if the form still shows the quote and the share it
    /// was asked for. A failure keeps the quote and the share, and says why.
    fn apply_bridge_quote(
        &mut self,
        operation: Option<ExecutorOperationId>,
        revision: u64,
        quoted: &Arc<SwapReview>,
        gas_share_bps: u16,
        result: Option<eyre::Result<SwapReview>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if form.operation != operation || form.quote_revision != revision {
            return;
        }
        let current = matches!(&form.quote, QuoteState::Ready(review)
            if Arc::ptr_eq(review, quoted)
                && strip_review(form, review).gas_share_bps() == gas_share_bps
                && form.quote_delivery() == Some(review.plan().delivery()))
            && matches!(
                self.form_mode(form),
                FormMode::Setup { .. } | FormMode::Order
            );
        let outcome = match result {
            Some(Ok(review)) => Ok(review),
            Some(Err(error))
                if matches!(
                    error.downcast_ref::<QuoteDeviationError>(),
                    Some(QuoteDeviationError::ExceedsThreshold)
                ) =>
            {
                let threshold = self
                    .swap_profile(cx)
                    .map_or(300, |profile| profile.anchor_deviation_bps());
                Err(format!(
                    "The bridge's rate is more than {} below the anchor price.",
                    format_bps_percent(u64::from(threshold))
                ))
            }
            Some(Err(error)) => {
                Err(bridge_unreachable(&error).unwrap_or_else(|| format!("{error:#}")))
            }
            None => Err("Updating the bridge quote stopped unexpectedly. Try again.".to_owned()),
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.quote_task = None;
        if !current {
            cx.notify();
            return;
        }
        match outcome {
            Ok(review) => {
                // The destination terms changed, so consent to the old ones doesn't carry over.
                form.price_acknowledged = false;
                form.high_costs_acknowledged = false;
                form.continuing_acknowledged = false;
                form.quote = QuoteState::Ready(Arc::new(Self::with_tracked_delivery(
                    &self.tracking,
                    operation,
                    review,
                )));
            }
            Err(error) => form.bridge_quote_error = Some(error.into()),
        }
        self.quote_installed(operation, window, cx);
    }

    /// `review` with the delivery allowance this swap's signing quotes showed its preview to
    /// lack. It takes the tracking alone, so the caller may hold the form.
    pub(super) fn with_tracked_delivery(
        tracking: &BTreeMap<ExecutorOperationId, SwapTracking>,
        operation: Option<ExecutorOperationId>,
        review: SwapReview,
    ) -> SwapReview {
        operation
            .and_then(|operation| tracking.get(&operation)?.delivery_shortfall)
            .and_then(|extra| review.with_delivery_shortfall(extra))
            .unwrap_or(review)
    }

    pub(super) fn apply_quote(
        &mut self,
        operation: Option<ExecutorOperationId>,
        revision: u64,
        result: Option<QuoteResult>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.operation != operation || form.quote_revision != revision {
            return;
        }
        form.quote_task = None;
        form.high_costs_acknowledged = false;
        form.continuing_acknowledged = false;
        let Some(result) = result else {
            form.quote =
                QuoteState::Failed(eyre::eyre!("Quoting stopped unexpectedly. Try again."));
            cx.notify();
            return;
        };
        if let Some(orderbook) = result.orderbook {
            form.set_orderbook(Some(orderbook.clone()), result.bridge_clients);
            if let Some(operation) = operation {
                self.tracking
                    .entry(operation)
                    .or_default()
                    .set_route(Some(orderbook));
            }
        }
        form.quoted_bridge = result.bridge;
        form.quote = match result.outcome {
            Ok(QuoteOutcome::TooLarge(largest)) => QuoteState::TooLarge { largest },
            Ok(QuoteOutcome::Review(review)) => {
                if review.price_verified() {
                    form.price_acknowledged = false;
                }
                // A share changed while this quote was in flight prices it locally. A share
                // the quote can't support keeps the Tight price the quote fell back to.
                let review = match review.with_gas_share(form.gas_share_bps) {
                    Ok(repriced)
                        if !form.bridged() && review.gas_share_bps() != form.gas_share_bps =>
                    {
                        Box::new(repriced)
                    }
                    _ => review,
                };
                // The quote names no receiver, so it takes the form's as it is now.
                QuoteState::Ready(Arc::new(Self::with_tracked_delivery(
                    &self.tracking,
                    operation,
                    review.with_receiver(form.quote_receiver()),
                )))
            }
            Ok(QuoteOutcome::PriceBlocked(block)) => QuoteState::PriceBlocked(block),
            Err(error) => QuoteState::Failed(error),
        };
        self.quote_installed(operation, window, cx);
    }

    /// Continue from the form's quote after a full quote or a bridge refresh ended.
    fn quote_installed(
        &mut self,
        operation: Option<ExecutorOperationId>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        // Only the swap's own form, in front, reopens the review. Otherwise the change stays
        // pending, and the form's Review… names it.
        let reopen = matches!(&form.quote, QuoteState::Ready(review)
            if form.review_problem(review).is_none())
            && operation.is_some_and(|operation| {
                self.reapproval
                    .is_some_and(|(pending, _)| pending == operation)
                    && self.swap_dialog_shows(operation, window, cx)
            });
        // Setup can confirm while its retry quote or bridge refresh is in flight. Replace that
        // setup preview with an order quote before offering approval.
        if self.form_mode(form) == FormMode::Order
            && matches!(&form.quote, QuoteState::Ready(review)
                if review.plan().swap_executor().requires_setup())
        {
            self.schedule_quote(window, cx);
            return;
        }
        // A Bridge share changed while the quote was in flight: quote its bridge leg again.
        if form.bridge_quote_error.is_none()
            && matches!(&form.quote, QuoteState::Ready(review)
                if review.bridge().is_some() && form.gas_share_pending(review))
        {
            self.schedule_bridge_quote(window, cx);
        }
        // The knob and the Minimum field follow the new quote's gas; the share stays.
        self.sync_gas_controls(window, cx);
        if reopen && let Some((_, change)) = self.reapproval {
            self.request_order_review(Some(change), window, cx);
        }
        cx.notify();
    }

    pub(super) fn request_order_review(
        &mut self,
        change: Option<SwapReviewChange>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(operation) = form.operation else {
            return;
        };
        let QuoteState::Ready(review) = &form.quote else {
            return;
        };
        let orderbook = form.orderbook.clone().or_else(|| {
            self.tracking
                .get(&operation)
                .and_then(|tracking| tracking.orderbook.clone())
        });
        let problem = form
            .review_problem(review)
            .or_else(|| self.destination_order_problem(form).map(Into::into))
            .or_else(|| {
                orderbook
                    .is_none()
                    .then_some("The swap's orderbook route isn't ready. Refresh the quote.".into())
            });
        let (Some(orderbook), None) = (orderbook, &problem) else {
            if let Some(form) = self.form.as_mut() {
                form.error = problem.map(|problem| problem.to_string());
            }
            cx.notify();
            return;
        };
        // A first order is bound to the approval saved with its setup, so the review names what
        // differs from it, such as another delivery, rather than the change that reopened the
        // form.
        let change = self.first_order_change(operation, review).or(change);
        let approval = OrderApproval {
            operation,
            review: Arc::clone(review),
            private_minimum: review.suggested_private_minimum(),
            price_acknowledged: form.price_acknowledged,
            orderbook,
            bridge: form.quoted_bridge.clone(),
            destination_minimum: review.bridge().map(|bridge| bridge.destination_minimum),
            // The review opens only once `review_problem` has this accepted.
            continuing_minimum: review.continuing_minimum(),
            full_review: true,
            // A speed-up belongs to its open order's swap use, which its plan names.
            swap_use: review
                .plan()
                .speed_up()
                .map(|speed_up| speed_up.swap_use)
                .or_else(|| form.reuse_use.filter(|_| form.reuse_account)),
            // A reused account's order to an existing destination account claims both first.
            pair_destination: form.destination_choice().filter(|_| form.reuse_account),
            speed_up: form.speed_up.as_ref().map(|open| open.uid),
        };
        // A review that names a change marks the amounts that differ from the saved approval.
        let approved = change.and_then(|_| self.approved_bounds(operation, review));
        let summary = self
            .swap_summary(
                &approval.review,
                None,
                change,
                approved,
                form.speed_up.as_ref(),
                cx,
            )
            .requiring_explicit_review();
        // The review about to open names the pending change.
        if self
            .reapproval
            .is_some_and(|(pending, _)| pending == operation)
        {
            self.reapproval = None;
        }
        self.request_authorization(SwapAction::Order(Box::new(approval)), summary, window, cx);
    }

    /// Why a private Bridge swap's order can't be reviewed yet: its stealth account on the
    /// destination network isn't confirmed as set up, which signing the order needs. `None`
    /// for any other swap.
    pub(super) fn destination_order_problem(&self, form: &SwapForm) -> Option<String> {
        if !form.private_bridge() {
            return None;
        }
        // Before a reused account's swap reserves its accounts there is nothing to wait for.
        let delivery = self.reserved_delivery(form)?;
        let network = network_name(delivery.destination_chain);
        match self.reserved_destination_progress(form)? {
            SwapSetupProgress::Done => None,
            SwapSetupProgress::NotSent | SwapSetupProgress::Failed => Some(format!(
                "The stealth account on {network} isn't set up. Retry its setup from the swap's details first."
            )),
            SwapSetupProgress::NetworkLoading => Some(format!(
                "Wait for {network} to load, so the stealth account there can be checked."
            )),
            SwapSetupProgress::Submitting | SwapSetupProgress::Pending => Some(format!(
                "Wait for the stealth account's setup on {network} to be confirmed."
            )),
        }
    }

    /// How `review` differs from the approval saved with the swap's setup, while that approval
    /// still binds the first order.
    pub(super) fn first_order_change(
        &self,
        operation: ExecutorOperationId,
        review: &SwapReview,
    ) -> Option<SwapReviewChange> {
        self.record(operation)
            .filter(|record| record.swap().is_none())
            .and_then(ExecutorRecord::swap_approval)
            .and_then(|approval| review.approval_change(approval))
    }

    /// The bounds of the approval saved with the swap's setup, while that approval still binds
    /// the first order and names `review`'s token pair and delivery.
    fn approved_bounds(
        &self,
        operation: ExecutorOperationId,
        review: &SwapReview,
    ) -> Option<&SwapApprovedBounds> {
        let plan = review.plan();
        let record = self
            .record(operation)
            .filter(|record| record.swap().is_none())?;
        let approval = record.swap_approval()?;
        (approval.delivery == plan.delivery()
            && record.swap_approval_tokens() == Some((plan.sell_token(), plan.buy_token())))
        .then_some(&approval.bounds)
    }
}
