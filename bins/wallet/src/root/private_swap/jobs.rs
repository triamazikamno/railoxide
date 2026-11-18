use super::*;

impl PrivateSwapsView {
    /// Whether this session's setup job covers `operation`. It sends both setups of a private
    /// Bridge swap.
    pub(super) fn submitting_setup(&self, operation: ExecutorOperationId) -> bool {
        self.job
            .as_ref()
            .is_some_and(|job| job.operation == operation && job.kind == SwapJobKind::Setup)
    }

    /// Whether `side`'s setup, sent by the running setup job of the swap `operation`, still
    /// waits for a broadcaster's answer after the re-sends that offer to send it again. Its job
    /// sends the swap's setups together and stops as one, so this holds only while no setup of
    /// that job is still before its broadcaster: each one waits for an answer or has ended.
    pub(super) fn setup_wait_outlasted(
        &self,
        operation: ExecutorOperationId,
        side: SetupSide,
    ) -> bool {
        let Some(tracking) = self
            .tracking
            .get(&operation)
            .filter(|_| self.submitting_setup(operation))
        else {
            return false;
        };
        tracking
            .setup_progress(side)
            .is_some_and(SetupProgress::wait_outlasted)
            && [SetupSide::Origin, SetupSide::Destination]
                .into_iter()
                .filter_map(|side| tracking.setup_progress(side))
                .all(|progress| {
                    progress.end().is_some()
                        || progress.stage()
                            == Some(TransactionGenerationStage::WaitingForBroadcasterResponse)
                })
    }

    /// Stop waiting for broadcaster answers in the running setup job of the swap `operation`,
    /// as its timeout would: its requests are no longer re-sent, and each setup that waited
    /// ends unanswered, which can still be included. A setup whose submission already ended
    /// was answered, and its answer ends with the job, so it keeps no stages and its record
    /// tells how it went. The swap still places its order once its setups are confirmed.
    pub(super) fn stop_setup_wait(&mut self, operation: ExecutorOperationId) {
        if !self.submitting_setup(operation) {
            return;
        }
        if let Some(job) = self.job.take() {
            job.abort.abort();
        }
        self.job_revision = self.job_revision.wrapping_add(1);
        for side in [SetupSide::Origin, SetupSide::Destination] {
            let answered = self
                .tracking
                .get(&operation)
                .and_then(|tracking| tracking.setup_progress(side))
                .and_then(SetupProgress::route)
                .is_some_and(|route| route.state == wallet_ops::SubmissionRouteState::Closed);
            self.finish_setup_progress(
                operation,
                side,
                (!answered).then_some(Ok(&PublicBroadcasterResultKind::TimedOut)),
            );
        }
    }

    /// Start a signing or submission job for one swap. Only one runs at a time.
    pub(super) fn start_job<T: Send + 'static>(
        &mut self,
        operation: ExecutorOperationId,
        kind: SwapJobKind,
        work: impl Future<Output = eyre::Result<T>> + Send + 'static,
        apply: impl FnOnce(&mut Self, T, &mut Window, &mut Context<'_, Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Debug UI fixture: a synthesized record has nothing to act on.
        if ui_fixture::holds_swaps() || self.job.is_some() || !self.session_is_current(cx) {
            return;
        }
        self.error = None;
        self.focus_active_dialog(window, cx);
        let tracking = self.tracking.entry(operation).or_default();
        tracking.error = None;
        tracking.status_checked = None;
        self.job_revision = self.job_revision.wrapping_add(1);
        let revision = self.job_revision;
        let join = self.runtime.spawn(work);
        self.job = Some(SwapJob {
            operation,
            kind,
            abort: join.abort_handle(),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.job_revision != revision || !this.session_is_current(cx) {
                    return;
                }
                // A setup job's swap may have moved to an account that replaced a used one.
                let operation = this.job.take().map_or(operation, |job| job.operation);
                this.reload_records();
                if kind == SwapJobKind::Check {
                    // A check may persist an outcome before a later RPC fails. Reconcile
                    // the destination on either result, just as routine bridge polls do.
                    this.reconcile_destination(operation, cx);
                }
                this.reload_destinations(cx);
                match result {
                    Ok(Ok(value)) => apply(this, value, window, cx),
                    Ok(Err(error))
                        if kind == SwapJobKind::Order
                            && this.lost_setup_inclusion(operation, &error) => {}
                    Ok(Err(error)) => {
                        let message = this.job_error(&error, cx);
                        this.fail(operation, message);
                    }
                    Err(error) if !error.is_cancelled() => this.fail(
                        operation,
                        "The local operation stopped unexpectedly. Check the swap before retrying."
                            .into(),
                    ),
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
        if kind == SwapJobKind::Order {
            self.show_detail(operation, window, cx);
        }
        cx.notify();
    }

    /// After an order job ended with `error`: when signing found the swap's own setup no
    /// longer included, forget this session's read that showed it, so the swap shows its setup
    /// as waiting again. A first order then waits to be placed by itself once the setup is
    /// included again, with no error left to hold it back; nothing was signed. A speed-up's
    /// failure is still told. Whether the setup was the reason.
    fn lost_setup_inclusion(
        &mut self,
        operation: ExecutorOperationId,
        error: &eyre::Report,
    ) -> bool {
        let unconfirmed = error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<wallet_ops::vault::SwapAccountRefusal>(),
                Some(wallet_ops::vault::SwapAccountRefusal::SetupUnconfirmed)
            )
        });
        // Only an order on the swap's own setup, which the record leaves unresolved, was
        // signed on its inclusion.
        let included_path = self
            .record(operation)
            .is_some_and(|record| wallet_ops::own_pending_swap_setup(record).is_some());
        if !unconfirmed || !included_path {
            return false;
        }
        let tracking = self.tracking.entry(operation).or_default();
        tracking.inclusion = None;
        if tracking
            .pending_order
            .as_ref()
            .is_some_and(|pending| pending.speeds_up)
        {
            return false;
        }
        tracking.pending_order = None;
        tracking.error = None;
        tracking.auto_place = true;
        true
    }

    pub(super) fn job_error(&self, error: &eyre::Report, cx: &gpui::App) -> String {
        self.setup_fee_error(error, self.origin_chain_id, "Continue", cx)
    }

    /// [`Self::job_error`] for a failed stage of the swap's detail, where Retry setup… opens
    /// the review of a new fee limit.
    pub(super) fn stage_error(&self, error: &eyre::Report, cx: &gpui::App) -> String {
        self.setup_fee_error(error, self.origin_chain_id, "Retry setup", cx)
    }

    /// What `error` says of a setup on `chain_id`. A setup fee that needs a new approval names
    /// what was approved and `control`, which opens the review of the new fee limit.
    pub(super) fn setup_fee_error(
        &self,
        error: &eyre::Report,
        chain_id: u64,
        control: &str,
        cx: &gpui::App,
    ) -> String {
        let root = self.root.upgrade();
        let registry = root
            .as_ref()
            .map(|root| &root.read(cx).effective_token_registry);
        if let Some(exceeded) = error.downcast_ref::<wallet_ops::ExecutorPrivateFeeLimitExceeded>()
        {
            return setup_fee_increased(
                chain_id,
                registry,
                exceeded.fee_token(),
                (exceeded.maximum(), exceeded.required()),
                control,
            );
        }
        if let Some(changed) = error.downcast_ref::<wallet_ops::ExecutorPrivateFeeTokenChanged>() {
            return setup_fee_token_changed(
                chain_id,
                registry,
                (changed.approved_token(), changed.approved_amount()),
                changed.fee_token(),
                control,
            );
        }
        format!("{error:#}")
    }

    pub(super) fn fail(&mut self, operation: ExecutorOperationId, message: String) {
        if let Some(form) = self
            .form
            .as_mut()
            .filter(|form| form.operation() == Some(operation))
        {
            form.set_error(message.clone());
        }
        let tracking = self.tracking.entry(operation).or_default();
        // A speed-up that failed placed no order, so the open one is the swap's order again.
        if tracking
            .pending_order
            .as_ref()
            .is_some_and(|pending| pending.speeds_up)
        {
            tracking.pending_order = None;
        }
        tracking.error = Some(message.clone());
        self.error = Some(message);
    }

    pub(super) fn request_authorization(
        &mut self,
        action: SwapAction,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let destination = self.authorized_destination(&action);
        let destination_chain = match &action {
            SwapAction::Setup(approval) => approval.destination_account().map(|(chain, _)| chain),
            // The swap's own setup is sent again through the swap's own session.
            SwapAction::SetupRetry(approval) => {
                (approval.side == SetupSide::Destination).then_some(approval.chain_id)
            }
            SwapAction::Order(approval) => approval
                .private_delivery()
                .map(|delivery| delivery.destination_chain),
            SwapAction::Cancel(_) => None,
        };
        let destination_session = match destination_chain {
            Some(chain) => match self.ready_destination(chain, cx) {
                Ok((session, _)) => Some(session),
                Err(error) => {
                    self.fail(action.operation(), error);
                    cx.notify();
                    return;
                }
            },
            None => None,
        };
        let Some(session) = self.private_session().cloned() else {
            return;
        };
        let command = Arc::new(SwapAuthorization {
            session,
            destination_session,
            action,
            destination,
        });
        let summary = authorization_summary(&command.action, summary);
        self.pending_authorization = Some(Arc::clone(&command));
        let view = cx.entity();
        // Reviews require explicit approval; the confirm-only step for an approved order
        // accepts a remembered spend authorization.
        let _ = self.root.update(cx, |root, cx| {
            if command.sessions_are_current(root) {
                root.request_spend_authorization(
                    SpendAuthorizationIntent::PrivateSwap(view, command),
                    summary,
                    window,
                    cx,
                );
            }
        });
        cx.notify();
    }

    /// The destination stealth account `action` also signs for: a private Bridge swap's, when
    /// its setup sets that account up, or its order pre-signs that account's shield.
    fn authorized_destination(&self, action: &SwapAction) -> Option<(u64, ExecutorOperationId)> {
        match action {
            SwapAction::Setup(approval) => approval.destination_account(),
            SwapAction::Order(approval) => {
                let delivery = approval.private_delivery()?;
                // An existing destination the order claims with its account before it signs.
                if let Some(destination) = approval.pair_destination {
                    return Some((destination.chain_id, destination.operation));
                }
                // The destination the order's own swap use names, not a later use's.
                let record = self.record(approval.operation)?;
                let swap_use = approval.swap_use.or_else(|| record.active_swap_use())?;
                let (_, operation) = model::swap_use_destination(record, swap_use)?;
                Some((delivery.destination_chain, operation))
            }
            // A setup sent again on the destination network is authorized there itself.
            SwapAction::SetupRetry(_) | SwapAction::Cancel(_) => None,
        }
    }

    pub(in crate::root) fn cancel_authorization(
        &mut self,
        command: &Arc<SwapAuthorization>,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .pending_authorization
            .as_ref()
            .is_some_and(|pending| Arc::ptr_eq(pending, command))
        {
            self.pending_authorization = None;
            cx.notify();
        }
    }

    /// Continue the action `command` asked authorization for. A private Bridge swap's setup or
    /// order also signs on its destination network: a hardware wallet's approval arrives with
    /// `destination`, the second authorization of the same device session, and a software
    /// wallet's `authorization` is duplicated for it.
    pub(in crate::root) fn continue_authorized(
        &mut self,
        command: &Arc<SwapAuthorization>,
        authorization: DesktopPrivateSpendAuthorization,
        destination: Option<DesktopPrivateSpendAuthorization>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .pending_authorization
            .take()
            .is_some_and(|current| Arc::ptr_eq(&current, command))
            || !self.session_is_current(cx)
            || !self
                .root
                .upgrade()
                .is_some_and(|root| command.sessions_are_current(root.read(cx)))
        {
            return;
        }
        // Debug UI fixture: a synthesized review places nothing.
        #[cfg(debug_assertions)]
        if let Some(refusal) = ui_fixture::review_refusal() {
            self.fail(command.action.operation(), refusal.to_owned());
            cx.notify();
            return;
        }
        let destination = match (destination, command.destination) {
            (_, None) => None,
            (Some(destination), Some(_)) => Some(destination),
            (None, Some(_)) => match authorization.for_destination() {
                Ok(destination) => Some(destination),
                Err(error) => {
                    self.fail(command.action.operation(), format!("{error:#}"));
                    cx.notify();
                    return;
                }
            },
        };
        match command.action.clone() {
            SwapAction::Setup(approval) => {
                self.submit_setup(*approval, authorization, destination, window, cx);
            }
            SwapAction::SetupRetry(approval) => {
                self.submit_setup_retry(*approval, authorization, window, cx);
            }
            SwapAction::Order(approval) => {
                self.submit_order(*approval, authorization, destination, window, cx);
            }
            SwapAction::Cancel(approval) => {
                self.submit_cancellation(*approval, authorization, window, cx);
            }
        }
    }

    /// The broadcaster network client for a paid setup or cancellation.
    pub(super) fn broadcaster_network(
        &self,
        cx: &mut Context<'_, Self>,
    ) -> Option<PublicBroadcasterWaku> {
        self.root
            .update(cx, |root, cx| {
                root.ensure_waku_for_delivery(crate::root::DeliveryMode::PublicBroadcaster, cx);
                root.public_broadcaster_waku()
            })
            .ok()
            .flatten()
    }

    /// Candidates and policy for paying a broadcaster on `chain_id` in `fee_token` for a swap's
    /// executor there.
    pub(super) fn broadcaster_candidates(
        &self,
        chain_id: u64,
        fee_token: Address,
        allow_out_of_range: bool,
        favorites_only: bool,
        cx: &gpui::App,
    ) -> Vec<PublicBroadcasterCandidate> {
        let Some(root) = self.root.upgrade() else {
            return Vec::new();
        };
        let root = root.read(cx);
        let Some(profile) = root
            .effective_chain_configs
            .get(chain_id)
            .and_then(wallet_ops::settings::EffectiveChainConfig::accepted_executor_profile)
        else {
            return Vec::new();
        };
        let policy = root.public_broadcaster_fee_policy(allow_out_of_range);
        wallet_ops::fee_policy_eligible_public_broadcasters(
            &crate::root::public_broadcaster::public_broadcaster_candidates_for_route(
                &root.monitor_fee_rows(),
                chain_id,
                fee_token,
                Some(profile),
                policy,
                root.public_broadcaster_anchor_cache
                    .cached_rate(chain_id, fee_token),
                &root.public_broadcaster_trust_filter(favorites_only),
            ),
            policy,
        )
    }
}
