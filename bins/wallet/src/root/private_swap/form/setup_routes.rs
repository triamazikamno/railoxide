use super::*;

impl PrivateSwapsView {
    // Setup route

    pub(super) fn start_setup_route_updates(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Debug UI fixture: its form sets nothing up, so no anchor or fee is read for it.
        if crate::root::private_swap::ui_fixture::holds_swaps() {
            return;
        }
        if let Some(root) = self.root.upgrade() {
            root.update(cx, |root, _| root.public_broadcaster_anchor_refresh.wake());
        }
        self.refresh_setup_route(cx);
        let task = cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL)
                    .await;
                let active = view
                    .update_in(cx, |view, window, cx| {
                        if !view.holds_setup_routes() || !view.session_is_current(cx) {
                            return false;
                        }
                        // Most ticks change nothing; redrawing the form for them costs a frame.
                        if view.update_setup_route(cx) == Some(true) {
                            cx.notify();
                        }
                        if view.form.is_some() {
                            view.refresh_destination_network(window, cx);
                        }
                        true
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        });
        // The holder's own route keeps the task, which ends with it.
        match (self.form.as_mut(), self.setup_retry.as_mut()) {
            (Some(form), _) => form.route.refresh_task = Some(task),
            (None, Some(retry)) => retry.route.refresh_task = Some(task),
            (None, None) => {}
        }
    }

    /// Whether something holds setup routes for the broadcaster controls to drive: the open
    /// form, or the open retry of one account's setup. A retry whose dialog is gone is
    /// dropped.
    fn holds_setup_routes(&mut self) -> bool {
        if self.form.is_some() {
            return true;
        }
        if self
            .setup_retry
            .as_ref()
            .is_some_and(|retry| !retry.is_open())
        {
            self.setup_retry = None;
        }
        self.setup_retry.is_some()
    }

    /// `side`'s setup route of the active holder: the open form's, or else the open retry's
    /// for the side it sends again.
    fn active_setup_route(&self, side: SetupSide) -> Option<&SetupRoute> {
        match &self.form {
            Some(form) => Some(form.setup_route(side)),
            None => self
                .setup_retry
                .as_ref()
                .filter(|retry| retry.side == side)
                .map(|retry| &retry.route),
        }
    }

    pub(super) fn active_setup_route_mut(&mut self, side: SetupSide) -> Option<&mut SetupRoute> {
        match &mut self.form {
            Some(form) => Some(form.setup_route_mut(side)),
            None => self
                .setup_retry
                .as_mut()
                .filter(|retry| retry.side == side)
                .map(|retry| &mut retry.route),
        }
    }

    /// The side whose route the broadcaster picker was opened for.
    fn setup_picker_side(&self) -> Option<SetupSide> {
        match &self.form {
            Some(form) => Some(form.broadcaster_side),
            None => self.setup_retry.as_ref().map(|retry| retry.side),
        }
    }

    /// The network of the stealth account `side` sets up for the active holder: as
    /// [`Self::setup_chain`] tells for the form, or the network of the retry's account.
    fn active_setup_chain(&self, side: SetupSide) -> Option<u64> {
        match &self.form {
            Some(form) => self.setup_chain(form, side),
            None => self
                .setup_retry
                .as_ref()
                .filter(|retry| retry.side == side)
                .map(|retry| retry.chain_id),
        }
    }

    /// The session and owner that estimate `side`'s setup fee for the active holder, on the
    /// account's own network, and the reserved account it is estimated for. A new account
    /// has none yet.
    fn setup_estimate_account(
        &self,
        side: SetupSide,
        cx: &App,
    ) -> Option<(
        Arc<WalletSession>,
        Arc<ExecutorOwner>,
        Option<ExecutorOperationId>,
    )> {
        // The destination network, for a setup that isn't the swap's own.
        let (destination, account) = match (&self.form, side) {
            (Some(form), SetupSide::Origin) => (None, form.operation),
            (Some(form), SetupSide::Destination) => (
                Some(self.destination_setup_chain(form)?),
                self.destination_operation(form),
            ),
            (None, _) => {
                let retry = self
                    .setup_retry
                    .as_ref()
                    .filter(|retry| retry.side == side)?;
                (
                    (side == SetupSide::Destination).then_some(retry.chain_id),
                    // A retired account's replacement isn't reserved yet.
                    (!self.retries_with_new_account(retry.operation, side))
                        .then_some(retry.account),
                )
            }
        };
        let (session, owner) = match destination {
            Some(chain_id) => self.destination_owner(chain_id, cx)?,
            None => (
                Arc::clone(self.private_session()?),
                Arc::clone(self.private_owner()?),
            ),
        };
        Some((session, owner, account))
    }

    /// Give focus back to the surface the setup route's controls sit on, where the
    /// broadcaster picker returns it: the form's amount, or the retry's dialog.
    fn focus_setup_route_holder(&self, window: &mut Window, cx: &mut App) {
        if self.form.is_some() {
            self.focus_form_amount(window, cx);
        } else if let Some(focus) = self
            .setup_retry
            .as_ref()
            .and_then(|retry| retry.focus.as_ref()?.upgrade())
        {
            focus.focus(window, cx);
        }
    }

    /// Another network's sync and private funds change outside the form. Read them again
    /// while the Buy picker lists them, and while Private balance depends on the form's
    /// network: once that network can or can't take it any more, the delivery is checked and
    /// quoted again.
    pub(super) fn refresh_destination_network(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.refresh_network_funding(cx);
        self.refresh_destination_accounts(window, cx);
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if form.picker.open {
            cx.notify();
        }
        if form.receive_to != ReceiveTo::PrivateBalance || form.network.is_none() || self.busy() {
            return;
        }
        let delivery = self.form_delivery(form, cx);
        if delivery.is_ok() != form.delivery.is_ok() {
            self.delivery_changed(window, cx);
        } else if delivery != form.delivery {
            // Only the reason changed, such as the sync's progress.
            if let Some(form) = self.form.as_mut() {
                form.delivery = delivery;
            }
            cx.notify();
        }
    }

    /// The fee tokens the private balance on `chain_id` can pay a setup broadcaster there with,
    /// the one selected, and its candidates. `preferred` is selected while `current` has no
    /// eligible broadcaster. Without one, the token worth the most in USD is, or the wrapped
    /// native token while no balance is priced.
    pub(in crate::root::private_swap) fn setup_fee_route(
        &self,
        chain_id: u64,
        preferred: Option<Address>,
        current: Option<Address>,
        allow_out_of_range: bool,
        favorites_only: bool,
        cx: &App,
    ) -> (
        Vec<PublicBroadcasterFeeTokenOption>,
        Option<Address>,
        Vec<PublicBroadcasterCandidate>,
    ) {
        let Some(root_entity) = self.root.upgrade() else {
            return (Vec::new(), None, Vec::new());
        };
        let root = root_entity.read(cx);
        let Some(profile) = root
            .effective_chain_configs
            .get(chain_id)
            .and_then(EffectiveChainConfig::accepted_executor_profile)
        else {
            return (Vec::new(), None, Vec::new());
        };
        let policy = root.public_broadcaster_fee_policy(allow_out_of_range);
        let trust = root.public_broadcaster_trust_filter(favorites_only);
        let rows = root.monitor_fee_rows();
        let options = root
            .chain_states
            .get(&chain_id)
            .and_then(|state| state.snapshot())
            .map(|snapshot| {
                public_broadcaster_fee_token_options_from_snapshot(
                    snapshot,
                    &rows,
                    Some(profile),
                    policy,
                    &trust,
                    Some(&root.effective_token_registry),
                    |token| {
                        root.public_broadcaster_anchor_cache
                            .cached_rate(chain_id, token)
                    },
                )
            })
            .unwrap_or_default();
        let token = options.first().map(|first| {
            let preferred = preferred
                .or_else(|| {
                    highest_value_public_broadcaster_fee_token(
                        &options,
                        root.effective_chain_configs
                            .get(chain_id)
                            .and_then(|chain| chain.wrapped_native_token),
                        |option| {
                            root.public_broadcaster_anchor_cache
                                .cached_token_usd_micro_value(
                                    chain_id,
                                    option.token,
                                    option.max_spendable,
                                )
                        },
                    )
                })
                .unwrap_or(first.token);
            resolve_selected_public_broadcaster_fee_token(
                current.unwrap_or(preferred),
                preferred,
                &options,
            )
        });
        let candidates = token
            .map(|token| {
                self.broadcaster_candidates(chain_id, token, allow_out_of_range, favorites_only, cx)
            })
            .unwrap_or_default();
        (options, token, candidates)
    }

    /// The network of the stealth account `side` sets up for the form's swap, while the
    /// form's review approves that setup. The swap's own side has one while its account is a
    /// new one, and the destination side while a private Bridge delivery's destination account
    /// needs its setup. An existing account has none: no fee is estimated for it, no
    /// broadcaster is chosen, and offers on its network don't concern the form.
    pub(super) fn setup_chain(&self, form: &SwapForm, side: SetupSide) -> Option<u64> {
        if form.public.is_some() {
            return (side == SetupSide::Destination)
                .then(|| self.destination_setup_chain(form))
                .flatten();
        }
        let mode = self.form_mode(form);
        match side {
            SetupSide::Origin => {
                matches!(mode, FormMode::Setup { .. }).then_some(self.origin_chain_id)
            }
            SetupSide::Destination => self.destination_setup_chain(form).filter(|_| {
                matches!(mode, FormMode::Setup { .. })
                    || (mode == FormMode::Order && form.reuse_account)
            }),
        }
    }

    /// The form's review approves a setup with the swap: of the swap's own stealth account,
    /// of a private Bridge swap's destination account, or of both.
    pub(super) fn reviews_setup(&self, form: &SwapForm) -> bool {
        self.setup_chain(form, SetupSide::Origin).is_some()
            || self.setup_chain(form, SetupSide::Destination).is_some()
    }

    /// The network of a private Bridge delivery's destination stealth account, while the
    /// form's review sets that account up: for a swap whose destination is a new account, and
    /// for a started one whose destination setup wasn't sent or failed. An existing account
    /// chosen as the destination takes no setup. A setup that is on its way or confirmed isn't
    /// sent again here. While the network isn't loaded, its records can't tell, so the setup
    /// stays part of the review until they are read.
    pub(super) fn destination_setup_chain(&self, form: &SwapForm) -> Option<u64> {
        let network = form.network.filter(|_| form.private_bridge())?;
        if form.destination_choice().is_some() {
            return None;
        }
        match self.reserved_destination_progress(form) {
            Some(
                SwapSetupProgress::Submitting
                | SwapSetupProgress::Pending
                | SwapSetupProgress::Done,
            ) => None,
            Some(
                SwapSetupProgress::NotSent
                | SwapSetupProgress::NetworkLoading
                | SwapSetupProgress::Failed,
            )
            | None => Some(network),
        }
    }

    /// How far the setup of the destination stealth account the form's swap reserved is.
    /// `None` before the swap reserves its accounts. A reused account's record tells of its
    /// draft's swap use, not of the account's earlier swaps.
    pub(super) fn reserved_destination_progress(
        &self,
        form: &SwapForm,
    ) -> Option<SwapSetupProgress> {
        if form.public.is_some() {
            return self.public_destination_setup_progress(form);
        }
        let (record, swap_use) = self.reserved_swap(form)?;
        if let Some(swap_use) = swap_use {
            let (delivery, _) =
                crate::root::private_swap::model::swap_use_destination(record, swap_use)?;
            return Some(self.swap_destination_setup_progress(
                crate::root::private_swap::model::SwapIdentity {
                    operation: record.operation(),
                    swap_use,
                },
                delivery,
            ));
        }
        let delivery = swap_private_delivery(record)?;
        Some(self.destination_setup_progress(record, delivery))
    }

    /// The destination stealth account the form's private Bridge swap reserved, which its own
    /// account's record links to: a started swap's, or the one the draft's swap use claimed a
    /// reused account with.
    pub(super) fn destination_operation(&self, form: &SwapForm) -> Option<ExecutorOperationId> {
        if let Some(public) = &form.public {
            return public.operation;
        }
        let (record, swap_use) = self.reserved_swap(form)?;
        if let Some(swap_use) = swap_use {
            return crate::root::private_swap::model::swap_use_destination(record, swap_use)
                .map(|(_, operation)| operation);
        }
        record.destination_operation()
    }

    pub(super) fn refresh_setup_route(&mut self, cx: &mut Context<'_, Self>) {
        if self.update_setup_route(cx).is_some() {
            cx.notify();
        }
    }

    /// Read the setup routes again: the swap's own network's, and a private Bridge swap's
    /// destination network's, each only while its account needs setup. A side without one
    /// keeps no route, so a broadcaster offer that appears or expires there changes nothing.
    /// An open retry has the one route of the account it sets up again. `None` when no setup
    /// route is there to update; otherwise whether anything the form, the retry or the
    /// broadcaster picker shows changed.
    pub(super) fn update_setup_route(&mut self, cx: &mut Context<'_, Self>) -> Option<bool> {
        if (self.form.is_none() && self.setup_retry.is_none()) || self.busy() {
            return None;
        }
        let sides = [SetupSide::Origin, SetupSide::Destination]
            .map(|side| (side, self.active_setup_chain(side)));
        if sides.iter().all(|(_, chain_id)| chain_id.is_none())
            && !self.form.as_ref().is_some_and(SwapForm::chooses_accounts)
        {
            return None;
        }
        let mut changed = false;
        for (side, chain_id) in sides {
            changed |= match chain_id {
                Some(chain_id) => self.update_route(side, chain_id, cx)?,
                // An existing account, or another delivery, leaves no account to set up.
                None => self
                    .active_setup_route_mut(side)
                    .is_some_and(SetupRoute::release),
            };
        }
        Some(changed)
    }

    /// Read `side`'s setup route on `chain_id` again, and estimate its fee when one is due.
    /// Whether anything the route's holder or the broadcaster picker shows changed.
    fn update_route(
        &mut self,
        side: SetupSide,
        chain_id: u64,
        cx: &mut Context<'_, Self>,
    ) -> Option<bool> {
        let route = self.active_setup_route(side)?;
        // Each setup is paid from the private balance on its own network: by default in the
        // token worth the most there.
        let (options, token, candidates) = self.setup_fee_route(
            chain_id,
            None,
            route.fee_token.filter(|_| route.fee_token_chosen),
            route.allow_out_of_range,
            route.favorites_only,
            cx,
        );
        let route = self.active_setup_route_mut(side)?;
        let quote_changed = route.estimate_candidate.as_ref().is_some_and(|quoted| {
            !candidates
                .iter()
                .any(|candidate| same_offer(quoted, candidate))
        });
        let token_changed = route.fee_token != token;
        let choice_changed = route.selected.as_ref().is_some_and(|address| {
            !candidates
                .iter()
                .any(|candidate| &candidate.railgun_address == address)
        });
        let shown_changed =
            route.fee_options != options || !same_shown_candidates(&route.candidates, &candidates);
        if choice_changed {
            route.selected = None;
        }
        route.fee_options = options;
        route.fee_token = token;
        route.candidates = candidates;
        if token_changed || choice_changed {
            route.invalidate_estimate();
        } else if quote_changed {
            route.renew_estimate();
        }
        let due = route.estimate_task.is_none()
            && (route.estimate.is_none() && route.estimate_error.is_none()
                || route
                    .next_estimate
                    .is_some_and(|next| Instant::now() >= next));
        if due {
            self.schedule_setup_estimate(side, cx);
        }
        Some(quote_changed || token_changed || choice_changed || shown_changed || due)
    }

    /// The broadcaster `route` pays as it stands: the one chosen, or one picked at random among
    /// its candidates under its fee and trust filters. `None` without an eligible one.
    pub(super) fn setup_route_candidate(
        &self,
        route: &SetupRoute,
        cx: &App,
    ) -> Option<PublicBroadcasterCandidate> {
        if route.candidates.is_empty() {
            return None;
        }
        let selection =
            route
                .selected
                .as_ref()
                .map_or(PublicBroadcasterSelection::Random, |address| {
                    PublicBroadcasterSelection::Specific {
                        railgun_address: address.clone(),
                    }
                });
        let root = self.root.upgrade()?;
        let root = root.read(cx);
        wallet_ops::select_public_broadcaster_with_policy_and_trust(
            &route.candidates,
            &selection,
            root.public_broadcaster_fee_policy(route.allow_out_of_range),
            &root.public_broadcaster_trust_filter(route.favorites_only),
        )
        .ok()
    }

    /// Estimate `side`'s setup fee with its network's own session and owner, which read the
    /// private balance the fee is paid from.
    fn schedule_setup_estimate(&mut self, side: SetupSide, cx: &mut Context<'_, Self>) {
        let Some(candidate) = self
            .active_setup_route(side)
            .and_then(|route| self.setup_route_candidate(route, cx))
        else {
            return;
        };
        // A setup sent again is estimated for its reserved account, from its own fee notes.
        let Some((session, owner, operation)) = self.setup_estimate_account(side, cx) else {
            return;
        };
        let Some(http) = self.http(cx) else {
            return;
        };
        let runtime = self.runtime.clone();
        let Some(route) = self.active_setup_route_mut(side) else {
            return;
        };
        route.estimate_revision = route.estimate_revision.wrapping_add(1);
        let revision = route.estimate_revision;
        route.estimate_candidate = Some(candidate.clone());
        // A repeated estimate keeps the last one on screen until it answers. A changed
        // broadcaster, fee token or choice invalidated it before this.
        route.estimate_error = None;
        route.estimate_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(COST_ESTIMATE_DEBOUNCE).await;
            let result = runtime
                .spawn(async move {
                    let client = http.operation_http_client().await?;
                    Box::pin(owner.estimate_swap_setup_fee(&session, operation, candidate, &client))
                        .await
                })
                .await;
            let result = result
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| format!("{error:#}")));
            let _ = view.update(cx, |view, cx| {
                let Some(route) = view.active_setup_route_mut(side) else {
                    return;
                };
                if route.estimate_revision != revision {
                    return;
                }
                route.estimate_task = None;
                route.next_estimate = Some(
                    Instant::now()
                        + if result.is_ok() {
                            Duration::from_secs(30)
                        } else {
                            Duration::from_secs(5)
                        },
                );
                match result {
                    Ok(estimate) => route.estimate = Some(estimate),
                    Err(error) => {
                        route.estimate = None;
                        route.estimate_error = Some(error);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The broadcaster picker's view of the setup route it was opened for.
    pub(in crate::root) fn setup_picker_context(&self) -> Option<RecoveryPickerContext> {
        let side = self.setup_picker_side()?;
        let route = self.active_setup_route(side)?;
        Some(RecoveryPickerContext {
            chain_id: self.active_setup_chain(side)?,
            token: route.fee_token?,
            choice: route.choice(),
            candidates: route.candidates.clone(),
            allow_out_of_range: route.allow_out_of_range,
            favorites_only: route.favorites_only,
            busy: self.busy(),
            estimating: route.estimate_task.is_some(),
            fee_context: route
                .estimate
                .as_ref()
                .map(BroadcasterPickerFeeEstimateContext::from),
        })
    }

    pub(in crate::root) fn set_setup_allow_out_of_range(
        &mut self,
        checked: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some(route) = self
            .setup_picker_side()
            .and_then(|side| self.active_setup_route_mut(side))
        else {
            return;
        };
        route.allow_out_of_range = checked;
        route.invalidate_estimate();
        self.refresh_setup_route(cx);
    }

    pub(in crate::root) fn choose_setup_broadcaster(
        &mut self,
        address: String,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() || !self.session_is_current(cx) {
            return;
        }
        self.refresh_setup_route(cx);
        let Some(side) = self.setup_picker_side() else {
            return;
        };
        let Some(route) = self.active_setup_route_mut(side) else {
            return;
        };
        if !route
            .candidates
            .iter()
            .any(|candidate| candidate.railgun_address == address)
        {
            return;
        }
        route.selected = Some(address);
        route.invalidate_estimate();
        self.schedule_setup_estimate(side, cx);
        self.focus_setup_route_holder(window, cx);
        cx.notify();
    }

    // Review, setup and submission

    pub(super) fn request_setup(
        &mut self,
        resume: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let waku = self.broadcaster_network(cx);
        let (approval, review) = match self.setup_approval(resume, waku) {
            Ok(approved) => approved,
            Err(error) => {
                if let Some(form) = self.form.as_mut() {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        let fees = approval.fees(self.origin_chain_id);
        let summary = self
            .swap_summary(&review, Some(&fees), None, None, None, cx)
            .requiring_explicit_review();
        self.request_authorization(SwapAction::Setup(Box::new(approval)), summary, window, cx);
    }

    /// The swap as reviewed: the quoted terms with the suggested minimum, and the setup's
    /// broadcaster route.
    pub(super) fn setup_approval(
        &self,
        resume: bool,
        waku: Option<PublicBroadcasterWaku>,
    ) -> Result<(SetupApproval, Arc<SwapReview>), String> {
        let form = self.form.as_ref().ok_or("The swap form closed.")?;
        let buy = form.quote_buy().ok_or("Choose the token to receive.")?;
        let review = match &form.quote {
            QuoteState::Ready(review)
                if review.plan().sell_token() == form.sell && review.plan().buy_token() == buy =>
            {
                Arc::clone(review)
            }
            _ => return Err("Wait for the quote.".into()),
        };
        if let Some(problem) = form.review_problem(&review) {
            return Err(problem.to_string());
        }
        let SetupParts {
            origin,
            destination,
            approval,
        } = self.setup_parts(form, &review, resume)?;
        let waku = waku.ok_or("Wait for the broadcaster network connection, then try again.")?;
        let operation = match form.operation {
            Some(operation) => operation,
            None => ExecutorOperationId::random().map_err(|error| error.to_string())?,
        };
        Ok((
            SetupApproval {
                operation,
                resume,
                sell: form.sell,
                buy,
                origin,
                waku,
                destination,
                swap_use: form.reuse_use,
                approval,
                orderbook: form.orderbook.clone(),
            },
            review,
        ))
    }

    /// What the form's setup review of `review` covers. Only an account the swap sets up has
    /// a setup, with the fee its route estimates: the swap's own while it is a new account,
    /// and a private Bridge swap's destination while that one is. The approval binds the
    /// accounts as they are chosen now and the fee limit of each setup.
    pub(super) fn setup_parts(
        &self,
        form: &SwapForm,
        review: &SwapReview,
        resume: bool,
    ) -> Result<SetupParts, String> {
        let origin = match self.setup_chain(form, SetupSide::Origin) {
            Some(_) => Some(Self::origin_setup(form)?),
            None => None,
        };
        let mut approval = review
            .approval(review.suggested_private_minimum(), form.price_acknowledged)
            .map_err(|error| format!("{error:#}"))?;
        let destination = match self.setup_chain(form, SetupSide::Destination) {
            Some(chain_id) => Some(self.destination_setup(form, chain_id)?),
            None => None,
        };
        let reused_destination = form.destination_choice();
        let saved = self.reserved_approval(form);
        // The approval binds the fee token and limit of each setup the swap needs: this
        // review's, or for a private Bridge swap's destination the one saved with a setup that
        // isn't sent again. An existing account takes no setup, so it binds none.
        approval.bounds.destination_setup_fee = match &destination {
            Some(destination) => Some(SwapSetupFeeLimit {
                token: destination.candidate.token,
                amount: destination.maximum_private_fee,
            }),
            None => saved
                .filter(|_| form.private_bridge() && reused_destination.is_none())
                .and_then(|saved| saved.bounds.destination_setup_fee),
        };
        approval.bounds.source_setup_fee = origin.as_ref().map(|origin| SwapSetupFeeLimit {
            token: origin.candidate.token,
            amount: origin.maximum_private_fee,
        });
        // The approval names the accounts as they are chosen now and whether each needs
        // setup. Reserving them binds their addresses. A resumed swap keeps the accounts its
        // saved approval binds.
        approval.accounts = if resume {
            saved.and_then(|saved| saved.accounts)
        } else {
            form.private_bridge().then(|| SwapApprovedAccounts {
                source: SwapApprovedAccount {
                    address: form
                        .operation
                        .and_then(|operation| self.record(operation))
                        .and_then(ExecutorRecord::address),
                    setup: origin.is_some(),
                },
                destination: Some(SwapApprovedAccount {
                    address: reused_destination.map(|account| account.address),
                    setup: destination.is_some(),
                }),
            })
        };
        let destination = destination
            .map(|setup| DestinationPlan::Setup(Box::new(setup)))
            .or_else(|| reused_destination.map(DestinationPlan::Existing));
        Ok(SetupParts {
            origin,
            destination,
            approval,
        })
    }

    /// The setup of the swap's own stealth account as the form's route estimates it.
    pub(super) fn origin_setup(form: &SwapForm) -> Result<OriginSetup, String> {
        let estimate = form
            .route
            .estimate
            .as_ref()
            .ok_or("Wait for the setup fee estimate.")?;
        let candidate = estimate.broadcaster();
        if !form
            .route
            .candidates
            .iter()
            .any(|current| same_offer(current, candidate))
        {
            return Err("The broadcaster quote changed. Wait for a new estimate.".into());
        }
        Ok(OriginSetup {
            candidate: candidate.clone(),
            maximum_private_fee: default_public_broadcaster_fee_limit(estimate.fee_amount()),
        })
    }

    /// The approval saved with the accounts the form's swap reserved: a started swap's, or
    /// the one the draft's swap use claimed a reused account with. An earlier swap of a
    /// reused account isn't the draft's.
    fn reserved_approval(&self, form: &SwapForm) -> Option<&SwapApproval> {
        let (record, swap_use) = self.reserved_swap(form)?;
        if let Some(swap_use) = swap_use {
            return record.swap_use(swap_use)?.approval();
        }
        record.swap_approval()
    }

    /// The setup of the destination stealth account on `chain_id` as the form's destination
    /// route estimates it: for a new account, or the started swap's reserved one.
    pub(super) fn destination_setup(
        &self,
        form: &SwapForm,
        chain_id: u64,
    ) -> Result<DestinationSetup, String> {
        let network = network_name(chain_id);
        let route = &form.destination_route;
        let estimate = route
            .estimate
            .as_ref()
            .ok_or_else(|| format!("Wait for the setup fee estimate on {network}."))?;
        let candidate = estimate.broadcaster();
        if !route
            .candidates
            .iter()
            .any(|current| same_offer(current, candidate))
        {
            return Err(format!(
                "The broadcaster quote on {network} changed. Wait for a new estimate."
            ));
        }
        let operation = match self.destination_operation(form) {
            Some(operation) => operation,
            None => ExecutorOperationId::random().map_err(|error| error.to_string())?,
        };
        Ok(DestinationSetup {
            chain_id,
            operation,
            candidate: candidate.clone(),
            maximum_private_fee: default_public_broadcaster_fee_limit(estimate.fee_amount()),
        })
    }

    /// Open the broadcaster picker for `side`'s setup route, on top of the swap dialog. The
    /// form's popover draws above dialogs, so it closes first, and focus moves to the amount,
    /// where the picker returns it. A retry's picker returns it to the retry's dialog.
    pub(super) fn choose_specific_setup_broadcaster(
        &mut self,
        side: SetupSide,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((chain_id, token)) = self.active_setup_chain(side).zip(
            self.active_setup_route(side)
                .and_then(|route| route.fee_token),
        ) else {
            return;
        };
        if let Some(form) = self.form.as_mut() {
            form.broadcaster_side = side;
            form.settings_open = false;
        }
        self.focus_setup_route_holder(window, cx);
        let target = BroadcasterPickerTarget::Swap(cx.weak_entity());
        let _ = self.root.update(cx, |root, cx| {
            root.open_broadcaster_picker_for_target(
                target,
                "Swap setup",
                chain_id,
                token,
                window,
                cx,
            );
        });
        cx.notify();
    }
}
