use super::*;

impl PrivateSwapsView {
    /// Reserve the stealth account, check that the approved amount still fits one order,
    /// persist the approval, and hand the setup to the broadcaster. Nothing is paid when the
    /// amount no longer fits. The order follows once the setup is confirmed.
    ///
    /// A private Bridge swap has a stealth account on each network, each a new one or an
    /// existing one, and the destination's goes through that network's session and owner
    /// with `destination_authorization`. Both are claimed for the swap together, so the saved
    /// approval names them, before anything is sent. Only a new account is set up: both
    /// setups are sent at the same time when both are new, and each result is reported by
    /// itself. A setup that failed is sent again by itself, for the pair already reserved.
    pub(in crate::root::private_swap) fn submit_setup(
        &mut self,
        approval: SetupApproval,
        authorization: DesktopPrivateSpendAuthorization,
        destination_authorization: Option<DesktopPrivateSpendAuthorization>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let operation = approval.operation;
        let (Some(owner), Some(session), Some(http)) = (
            self.private_owner().cloned(),
            self.private_session().cloned(),
            self.http(cx),
        ) else {
            return;
        };
        let destination = match approval.destination.clone() {
            Some(plan) => {
                let (chain_id, _) = plan.identity();
                let submission =
                    self.ready_destination(chain_id, cx)
                        .and_then(|(session, owner)| {
                            Ok(DestinationSubmission {
                                plan,
                                session,
                                owner,
                                authorization: destination_authorization
                                    .ok_or(DESTINATION_AUTHORIZATION_MISSING)?,
                            })
                        });
                match submission {
                    Ok(submission) => Some(submission),
                    Err(error) => {
                        self.fail(operation, error.clone());
                        // A new swap has no record yet, so its form says why nothing was sent.
                        if let Some(form) = self.form.as_mut() {
                            form.set_error(error);
                        }
                        cx.notify();
                        return;
                    }
                }
            }
            None => None,
        };
        let networks = destination.as_ref().map(|destination| {
            (
                self.chain_label(),
                network_name(destination.plan.identity().0),
            )
        });
        // A destination setup's fee is in a token of its own network.
        let destination_chain = destination
            .as_ref()
            .map_or(self.origin_chain_id, |destination| {
                destination.plan.identity().0
            });
        // A new attempt starts for each side whose setup is sent. A side that sends none keeps
        // no progress of an earlier one.
        let sends_destination = destination
            .as_ref()
            .is_some_and(|destination| destination.plan.setup().is_some());
        self.tracking
            .entry(operation)
            .or_default()
            .clear_setup_progress();
        let (progress, route_tx) = approval
            .origin
            .as_ref()
            .map(|origin| {
                self.start_setup_progress(
                    operation,
                    SetupSide::Origin,
                    origin.candidate.clone(),
                    cx,
                )
            })
            .unzip();
        let (destination_progress, destination_route_tx) = destination
            .as_ref()
            .and_then(|destination| destination.plan.setup())
            .map(|setup| setup.candidate.clone())
            .map(|candidate| {
                self.start_setup_progress(operation, SetupSide::Destination, candidate, cx)
            })
            .unzip();
        let bounds = &approval.approval.bounds;
        // An existing account's swap has no order yet for its progress to show, so its draft
        // stands in until the order is placed, and reopens the form with these terms.
        let draft = approval
            .swap_use
            .filter(|_| approval.origin.is_none())
            .map(|swap_use| crate::root::private_swap::PendingSwapOrder {
                previous_order: self
                    .record(operation)
                    .and_then(|record| {
                        crate::root::private_swap::model::swap_use_last_order(record, swap_use)
                    })
                    .map(wallet_ops::vault::SwapOrderRecord::uid),
                sell: approval.sell,
                buy: approval.buy,
                delivery: approval.approval.delivery.clone(),
                amount: bounds.spend_amount(),
                private_minimum: bounds.private_minimum,
                slippage_bps: bounds.slippage_bps,
                gas_share_bps: bounds.gas_share_bps.unwrap_or(GAS_SHARE_BALANCED_BPS),
                valid_for: bounds.valid_for_secs.map_or_else(
                    || self.default_valid_for(cx),
                    |secs| Duration::from_secs(secs.into()),
                ),
                reuse_account: true,
                swap_use,
                started_at: now_unix(),
                speeds_up: false,
            });
        let tracking = self.tracking.entry(operation).or_default();
        tracking.amount = Some(bounds.spend_amount());
        tracking.slippage_bps = Some(bounds.slippage_bps);
        tracking.gas_share_bps = bounds.gas_share_bps;
        tracking.valid_for = bounds
            .valid_for_secs
            .map(|secs| Duration::from_secs(secs.into()));
        if approval.orderbook.is_some() {
            tracking.set_route(approval.orderbook.clone());
        }
        // An existing account keeps what this session observed of its own setup.
        if approval.origin.is_some() {
            tracking.setup = None;
            tracking.cursor = None;
        }
        if sends_destination {
            tracking.destination_setup = None;
        }
        if draft.is_some() {
            tracking.pending_order = draft;
        }
        tracking.error = None;
        // The order was approved with the setup: place it as soon as every setup the swap
        // needs is confirmed, whichever of its accounts is the new one.
        tracking.auto_place = true;
        let byte_budget = tracking.byte_budget;
        // The setups' account reads go over the swap's route, which its order keeps.
        let route = self.swap_route(&http, operation);
        // A swap whose own account was replaced before keeps the use it started with, which
        // isn't its new account's first.
        let reserved_use = self
            .record(operation)
            .and_then(ExecutorRecord::active_swap_use);
        let (replacements, followed) = Self::follow_replacements(cx);
        self.start_job(
            operation,
            SwapJobKind::Setup,
            async move {
                let SetupApproval {
                    resume,
                    sell,
                    buy,
                    origin,
                    waku,
                    swap_use,
                    approval: mut saved,
                    ..
                } = approval;
                let swap_use = swap_use
                    .or(reserved_use)
                    .unwrap_or_else(|| SwapUseId::first(operation));
                // A newly reserved account that is found used is replaced, and the swap goes
                // on with the new one. The view is told at once, and again with the result.
                let mut replaced = Vec::new();
                let mut accounts = SwapAccountReplacements::new(
                    owner.as_ref(),
                    destination.as_ref().map(|destination| {
                        (destination.owner.as_ref(), destination.plan.identity().1)
                    }),
                    operation,
                    swap_use,
                    |from, swap| {
                        let account = ReplacedSwapAccount { from, swap };
                        replaced.push(account);
                        let _ = replacements.send(account);
                    },
                );
                let outcome = Box::pin(async {
                    let orderbook = route.orderbook(&owner).await?;
                    let client = orderbook.http();
                    // A destination account that can't be reserved again doesn't hold back the
                    // swap's own setup.
                    let (mut prepared, prepared_destination) = match &destination {
                        // The pair is reserved: each account that still needs setup is prepared
                        // again by itself.
                        Some(destination) if resume => {
                            let prepared = resume_pair_side(
                                &mut accounts,
                                SwapUseSide::Source,
                                &owner,
                                operation,
                                origin.as_ref().map(|origin| &origin.candidate),
                                &authorization,
                                client,
                            )
                            .await?;
                            let prepared_destination = resume_pair_side(
                                &mut accounts,
                                SwapUseSide::Destination,
                                &destination.owner,
                                destination.plan.identity().1,
                                destination.plan.setup().map(|setup| &setup.candidate),
                                &destination.authorization,
                                client,
                            )
                            .await;
                            (prepared, Some(prepared_destination))
                        }
                        // Both accounts are claimed for the swap in one write, each as the new
                        // or existing account it was chosen as. The saved approval then binds
                        // both addresses, with the destination account as its delivery's
                        // receiver.
                        Some(destination) => {
                            let account = |new: bool, operation| {
                                if new {
                                    SwapAccountChoice::New(operation)
                                } else {
                                    SwapAccountChoice::Existing(operation)
                                }
                            };
                            let prepared = Box::pin(accounts.prepare_pair(SwapPairPreparation {
                                use_id: swap_use,
                                source: account(origin.is_some(), operation),
                                destination: Some(account(
                                    destination.plan.setup().is_some(),
                                    destination.plan.identity().1,
                                )),
                                approval: saved,
                                candidate: origin.as_ref().map(|origin| origin.candidate.clone()),
                                destination_candidate: destination
                                    .plan
                                    .setup()
                                    .map(|setup| setup.candidate.clone()),
                                authorization: &authorization,
                                destination_authorization: Some(&destination.authorization),
                                client,
                            }))
                            .await?;
                            saved = prepared.approval;
                            (prepared.origin, prepared.destination.map(Ok))
                        }
                        None if resume => (
                            resume_pair_side(
                                &mut accounts,
                                SwapUseSide::Source,
                                &owner,
                                operation,
                                origin.as_ref().map(|origin| &origin.candidate),
                                &authorization,
                                client,
                            )
                            .await?,
                            None,
                        ),
                        // The new record holds the approval from its first write.
                        None => {
                            let origin = origin.as_ref().ok_or_else(|| {
                                eyre::eyre!("This swap has no setup to send. Review it again.")
                            })?;
                            let prepared = Box::pin(owner.prepare_swap_setup(
                                operation,
                                origin.candidate.clone(),
                                saved.clone(),
                                None,
                                &authorization,
                                client,
                            ))
                            .await;
                            (
                                SwapPairSide::Setup(
                                    Box::pin(accounts.prepare_setup(
                                        SwapUseSide::Source,
                                        prepared,
                                        &origin.candidate,
                                        &authorization,
                                        client,
                                    ))
                                    .await?,
                                ),
                                None,
                            )
                        }
                    };
                    // A new account is planned from its reservation, and an existing one from
                    // its recorded state, as its quote was.
                    let executor = match &prepared {
                        SwapPairSide::Setup(prepared) => SwapExecutor::reserved(prepared)?,
                        SwapPairSide::Existing { operation, .. } => {
                            owner.swap_order_preview(*operation, true, None)?
                        }
                    };
                    let request = SwapAmountRequest {
                        sell_token: sell,
                        buy_token: buy,
                        amount: saved.bounds.spend_amount(),
                        delivery: saved.delivery.clone(),
                        byte_budget,
                    };
                    let planning_owner = Arc::clone(&owner);
                    let planning_session = Arc::clone(&session);
                    let plan = tokio::task::spawn_blocking(move || {
                        planning_owner.plan_swap_amount(&executor, &planning_session, &request)
                    })
                    .await
                    .map_err(|_| eyre::eyre!("Planning the swap stopped unexpectedly."))??;
                    if matches!(plan, SwapAmountPlan::TooLarge { .. }) {
                        return Ok(SetupOutcome::TooLarge);
                    }
                    // A resumed setup's record keeps its earlier approval until the user
                    // authorized this review. A reserved pair's approval then binds the accounts
                    // as they are derived by now: a preparation interrupted before the
                    // destination account was derived left the delivery's placeholder receiver
                    // in the saved approval. It names a replaced account's replacement before
                    // any setup is sent, also when this job sends no destination setup.
                    if resume {
                        accounts.save_resumed_approval(saved)?;
                    }
                    // Only a new account's setup is sent and paid. An existing account has none.
                    // A replacement account's setup is sent with the same request.
                    let origin_request = origin.as_ref().map(|origin| SwapSetupRequest {
                        maximum_private_fee: origin.maximum_private_fee,
                        session: Arc::clone(&session),
                        authorization: &authorization,
                        waku: waku.clone(),
                        verify_proof: true,
                        progress_tx: progress,
                        route_tx,
                        response_timeout: SWAP_BROADCASTER_RESPONSE_TIMEOUT,
                        republish_interval: SWAP_BROADCASTER_REPUBLISH_INTERVAL,
                        client: client.clone(),
                    });
                    let (origin, destination) = match (&destination, prepared_destination) {
                        (Some(destination), Some(Ok(mut prepared_destination))) => {
                            let destination_request =
                                destination.plan.setup().map(|setup| SwapSetupRequest {
                                    maximum_private_fee: setup.maximum_private_fee,
                                    session: Arc::clone(&destination.session),
                                    authorization: &destination.authorization,
                                    waku: waku.clone(),
                                    verify_proof: true,
                                    progress_tx: destination_progress,
                                    route_tx: destination_route_tx,
                                    response_timeout: SWAP_BROADCASTER_RESPONSE_TIMEOUT,
                                    republish_interval: SWAP_BROADCASTER_REPUBLISH_INTERVAL,
                                    client: client.clone(),
                                });
                            Box::pin(accounts.submit_setups(
                                (&mut prepared, origin_request),
                                Some((&mut prepared_destination, destination_request)),
                            ))
                            .await
                        }
                        (_, failed) => {
                            // No setup call closes the destination's route here.
                            if let Some(route_tx) = &destination_route_tx {
                                route_tx.send_modify(|route| {
                                    route.state = wallet_ops::SubmissionRouteState::Closed;
                                });
                            }
                            let (origin, _) = Box::pin(
                                accounts.submit_setups((&mut prepared, origin_request), None),
                            )
                            .await;
                            (origin, failed.and_then(Result::err).map(Err))
                        }
                    };
                    Ok::<_, eyre::Report>(SetupOutcome::Sent {
                        origin: origin.map(|sent| sent.map(|outcome| outcome.result)),
                        destination: destination.map(|sent| sent.map(|outcome| outcome.result)),
                    })
                })
                .await;
                drop(accounts);
                Ok((replaced, outcome))
            },
            move |this, (replaced, outcome), window, cx| {
                // The swap is the one its last replacement left. The view follows here what
                // it didn't while the job ran, also when the job failed after a replacement.
                let operation = replaced
                    .last()
                    .map_or(operation, |account| account.swap.operation);
                for account in replaced.into_iter().skip(followed.finish()) {
                    this.follow_replaced_account(account, cx);
                }
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        this.fail_setup_job(operation, &error, cx);
                        return;
                    }
                };
                let tracking = this.tracking.entry(operation).or_default();
                match outcome {
                    SetupOutcome::Sent {
                        origin,
                        destination,
                    } => {
                        // Each side ends with its own answer. A side without one sent nothing.
                        let ended = [
                            (
                                SetupSide::Origin,
                                origin.as_ref().map(|sent| {
                                    sent.as_ref().map_err(|error| this.stage_error(error, cx))
                                }),
                            ),
                            (
                                SetupSide::Destination,
                                destination.as_ref().map(|sent| {
                                    sent.as_ref().map_err(|error| {
                                        this.setup_fee_error(
                                            error,
                                            destination_chain,
                                            "Retry setup",
                                            cx,
                                        )
                                    })
                                }),
                            ),
                        ];
                        // A result the swap's detail shows on its setup's stages isn't told
                        // again as the swap's error. Each side's result is judged by itself.
                        let [origin_shown, destination_shown] = ended.map(|(side, result)| {
                            this.finish_setup_progress(operation, side, result)
                                && this.shows_setup_progress(operation, side, cx)
                        });
                        let failed = origin.as_ref().is_some_and(Result::is_err);
                        let origin = origin.and_then(|sent| match sent {
                            Ok(result) => broadcaster_result_problem(&result, "setup"),
                            Err(error) => Some(this.job_error(&error, cx)),
                        });
                        let destination = destination.and_then(|sent| match sent {
                            Ok(result) => broadcaster_result_problem(&result, "setup"),
                            Err(error) => Some(this.setup_fee_error(
                                &error,
                                destination_chain,
                                "Continue",
                                cx,
                            )),
                        });
                        let problem =
                            setup_problems(origin.clone(), destination.clone(), networks.as_ref());
                        let unshown = setup_problems(
                            origin.filter(|_| !origin_shown),
                            destination.filter(|_| !destination_shown),
                            networks.as_ref(),
                        );
                        // A setup that wasn't handed off fails as before, also in the form.
                        match problem {
                            Some(problem) if failed => {
                                this.report_setup_failure(operation, problem, unshown);
                            }
                            _ => this.tracking.entry(operation).or_default().error = unshown,
                        }
                    }
                    SetupOutcome::TooLarge => {
                        tracking.auto_place = false;
                        tracking.clear_setup_progress();
                        // The form offers the largest amount that fits, for a new review.
                        if !window.has_active_dialog(cx)
                            || this.detail_is_active(operation, window, cx)
                        {
                            this.open_existing_form(operation, window, cx);
                        }
                        this.fail(operation, "Your notes changed after the quote, and the approved amount no longer fits one swap. Nothing was paid.".into());
                    }
                }
                cx.notify();
            },
            window,
            cx,
        );
        // The detail follows the setup and then the order it places.
        self.show_detail(operation, window, cx);
    }

    /// Follow the replacements a setup job reports while it runs: the job sends each one as
    /// it makes it, and the swap moves at once. The job's completion ends the following and
    /// follows what was left unread.
    pub(super) fn follow_replacements(
        cx: &Context<'_, Self>,
    ) -> (
        tokio::sync::mpsc::UnboundedSender<ReplacedSwapAccount>,
        FollowedReplacements,
    ) {
        let (reports, mut replacements) = tokio::sync::mpsc::unbounded_channel();
        let followed = Rc::new(Cell::new(0));
        let count = Rc::clone(&followed);
        let following = cx.spawn(async move |view, cx| {
            while let Some(replaced) = replacements.recv().await {
                let moved = view.update(cx, |this, cx| {
                    this.follow_replaced_account(replaced, cx);
                    count.set(count.get() + 1);
                });
                if moved.is_err() {
                    break;
                }
            }
        });
        (
            reports,
            FollowedReplacements {
                followed,
                _following: following,
            },
        )
    }

    /// Move a swap to the stealth account that replaced a used one. The swap's own account's
    /// operation names it in this view, so when that account was replaced, everything held
    /// under the retired operation moves to the new one in one update: the tracking with each
    /// setup's stages, which a running setup keeps reporting into, the destination account
    /// read for it, the running job, the open detail, and an open form or setup retry of the
    /// swap. The records are then read again, which also shows a replaced destination
    /// account. Following a replacement a second time changes nothing.
    pub(in crate::root::private_swap) fn follow_replaced_account(
        &mut self,
        replaced: ReplacedSwapAccount,
        cx: &mut Context<'_, Self>,
    ) {
        let ReplacedSwapAccount { from, swap } = replaced;
        let to = swap.operation;
        if from != to {
            if let Some(tracking) = self.tracking.remove(&from) {
                self.tracking.insert(to, tracking);
            }
            let identity = |operation| crate::root::private_swap::model::SwapIdentity {
                operation,
                swap_use: swap.swap_use,
            };
            if let Some(destination) = self.destinations.remove(&identity(from)) {
                self.destinations.insert(identity(to), destination);
            }
            if let Some(job) = self.job.as_mut().filter(|job| job.operation == from) {
                job.operation = to;
            }
            if let Some(dialog) = self
                .dialog
                .as_mut()
                .filter(|dialog| dialog.view == SwapDialogView::Detail(from))
            {
                dialog.view = SwapDialogView::Detail(to);
            }
            if let Some(form) = self.form.as_mut() {
                for operation in [&mut form.operation, &mut form.back_to_detail] {
                    if *operation == Some(from) {
                        *operation = Some(to);
                    }
                }
            }
            if let Some(retry) = self.setup_retry.as_mut() {
                for operation in [&mut retry.operation, &mut retry.account] {
                    if *operation == from {
                        *operation = to;
                    }
                }
            }
        }
        self.reload_records();
        self.reload_destinations(cx);
        cx.notify();
    }

    /// Start a new attempt's progress for `side` of the swap `operation`, sent to
    /// `broadcaster`, in place of what an earlier attempt left, and return the senders its
    /// setup reports its stages and its route through. The view redraws on each stage and each
    /// change of the route.
    pub(super) fn start_setup_progress(
        &mut self,
        operation: ExecutorOperationId,
        side: SetupSide,
        broadcaster: PublicBroadcasterCandidate,
        cx: &Context<'_, Self>,
    ) -> (
        tokio::sync::watch::Sender<TransactionGenerationStage>,
        wallet_ops::SubmissionRouteSender,
    ) {
        let (progress, receiver) =
            tokio::sync::watch::channel(TransactionGenerationStage::SelectingPrivateNotes);
        let (route, route_receiver) =
            tokio::sync::watch::channel(wallet_ops::SubmissionRouteProgress::default());
        let mut changes = receiver.clone();
        let mut route_changes = route_receiver.clone();
        let waiting_since = Rc::new(Cell::new(None));
        let waiting = Rc::clone(&waiting_since);
        // While a broadcaster's answer is awaited, its re-sends and time left are redrawn each
        // second, as a private action's progress redraws them.
        let watch = cx.spawn(async move |view, cx| {
            loop {
                if *changes.borrow_and_update()
                    == TransactionGenerationStage::WaitingForBroadcasterResponse
                {
                    if waiting.get().is_none() {
                        waiting.set(Some(Instant::now()));
                    }
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    if changes.has_changed().is_err() {
                        break;
                    }
                } else {
                    // A closed route watch leaves the stage watch alone to wake the view.
                    let changed = tokio::select! {
                        changed = changes.changed() => changed,
                        Ok(()) = route_changes.changed() => Ok(()),
                    };
                    if changed.is_err() {
                        break;
                    }
                }
                if view.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        let tracking = self.tracking.entry(operation).or_default();
        *tracking.setup_progress_mut(side) = Some(
            SetupProgress::running(broadcaster, receiver, route_receiver, Some(watch))
                .waiting_since(waiting_since),
        );
        (progress, route)
    }

    /// End the progress of `side`'s setup with its job's result: a broadcaster's answer, or
    /// the error it failed with. With no result its setup wasn't sent, and it keeps none.
    /// `true` when this attempt's stages took the result. A side without stages takes none,
    /// and neither do stages that had already ended.
    pub(in crate::root::private_swap) fn finish_setup_progress(
        &mut self,
        operation: ExecutorOperationId,
        side: SetupSide,
        result: Option<Result<&PublicBroadcasterResultKind, String>>,
    ) -> bool {
        let Some(tracking) = self.tracking.get_mut(&operation) else {
            return false;
        };
        let progress = tracking.setup_progress_mut(side);
        let Some(result) = result else {
            *progress = None;
            return false;
        };
        let recorded = progress
            .as_mut()
            .is_some_and(|progress| progress.finish(result));
        // A setup that failed before its first stage has no stages to keep.
        if !recorded
            && progress
                .as_ref()
                .is_some_and(|progress| progress.end().is_none())
        {
            *progress = None;
        }
        recorded
    }

    /// Fail the swap with the error its setup job stopped at, as `start_job` does for a job
    /// without setup progress, after ending each side's setup that still ran with it. An error
    /// the swap's detail shows on a setup's stages is told there, and in the swap's open form.
    pub(super) fn fail_setup_job(
        &mut self,
        operation: ExecutorOperationId,
        error: &eyre::Report,
        cx: &App,
    ) {
        let message = self.job_error(error, cx);
        // A stage names the control beside it, Retry setup…, where the form names Continue.
        let stage = self.stage_error(error, cx);
        let mut shown = false;
        for side in [SetupSide::Origin, SetupSide::Destination] {
            shown |= self.finish_setup_progress(operation, side, Some(Err(stage.clone())))
                && self.shows_setup_progress(operation, side, cx);
        }
        // A job that retired its account ended the swap there, which the swap's stage tells.
        let retired = self
            .record(operation)
            .is_some_and(ExecutorRecord::is_retired);
        let unshown = (!shown && !retired).then(|| message.clone());
        self.report_setup_failure(operation, message, unshown);
    }

    /// Tell a setup's `problem` where the swap shows it. The swap's open form says all of it.
    /// The swap's error takes `unshown`, the part no setup's stages show in the swap's detail,
    /// so the detail doesn't tell a stage's problem a second time.
    fn report_setup_failure(
        &mut self,
        operation: ExecutorOperationId,
        problem: String,
        unshown: Option<String>,
    ) {
        if let Some(unshown) = unshown {
            self.fail(operation, unshown);
        }
        if let Some(form) = self
            .form
            .as_mut()
            .filter(|form| form.operation() == Some(operation))
        {
            form.set_error(problem);
        }
    }

    // Quote and order

    pub(super) fn retry_quote(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        // An explicit retry gets a new isolated route instead of reusing a failed circuit.
        form.set_orderbook(None, None);
        if let Some(tracking) = form
            .operation
            .and_then(|operation| self.tracking.get_mut(&operation))
        {
            tracking.set_route(None);
        }
        self.schedule_quote(window, cx);
    }
}
