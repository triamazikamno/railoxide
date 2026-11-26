use super::*;

impl PrivateSwapsView {
    pub(in crate::root) fn has_shown_swaps(&self) -> bool {
        self.shown_swaps().next().is_some() || self.shown_public_swaps().next().is_some()
    }

    /// The one card for every unfinished swap, in the Private tab's pending-status slot.
    pub(super) fn render_card(&self, view: &Entity<Self>, cx: &App) -> Option<gpui::Div> {
        if !self.session_is_current(cx) {
            return None;
        }
        let swaps = self
            .shown_swaps()
            .map(|(record, stage)| (stage, self.labels(record, cx)))
            .collect::<Vec<_>>();
        let line = card_line(&swaps, self.public_card_line(cx))?;
        let view = view.clone();
        let action = app_button("wallet-private-swaps-details", "Details…")
            .ghost()
            .xsmall()
            .compact()
            .on_click(move |_, window, cx| {
                view.update(cx, |view, cx| view.open_details(window, cx));
            });
        let detail = Some(SharedString::from(line.detail));
        Some(if line.attention {
            ui::private_assets::private_attention_status(line.title, detail, action)
        } else {
            ui::private_assets::private_pending_status(line.title, detail, action)
        })
    }

    /// One shown swap opens its detail; several open My orders, filtered to Open.
    pub(in crate::root::private_swap) fn open_details(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let shown = self
            .shown_swaps()
            .map(|(record, _)| record.operation())
            .collect::<Vec<_>>();
        let public = self.shown_public_swaps().collect::<Vec<_>>();
        if let ([], [(identity, _)]) = (shown.as_slice(), public.as_slice()) {
            self.show_view(SwapDialogView::PublicDetail(*identity), window, cx);
            return;
        }
        if !public.is_empty() {
            // Several swaps open My orders filtered to Open, as private swaps alone do, unless
            // a shown Public-paid swap is listed under another group, which Open would hide.
            self.orders_filter = public
                .iter()
                .all(|(_, stage)| stage.group() == SwapOrderGroup::Open)
                .then_some(SwapOrderGroup::Open);
            self.show_view(SwapDialogView::Orders, window, cx);
            return;
        }
        match shown.as_slice() {
            [] => {}
            [operation] => self.show_detail(*operation, window, cx),
            _ => {
                self.orders_filter = Some(SwapOrderGroup::Open);
                self.show_view(SwapDialogView::Orders, window, cx);
            }
        }
    }

    pub(in crate::root::private_swap) fn progress_title(
        &self,
        operation: ExecutorOperationId,
        cx: &App,
    ) -> String {
        // A cancelled preparation keeps its swap's name, whatever its account shows next.
        if let Some(cancelled) = self.cancelled_preparation(operation) {
            return cancelled.title.clone();
        }
        self.record(operation).map_or_else(
            || "Swap".into(),
            |record| format!("Swap {}", self.labels(record, cx).pair),
        )
    }

    /// One swap's order detail: its steps, facts and outcome, and a footer with the actions its
    /// state allows. Until every account the swap sets up is ready, the steps are the setup's,
    /// and the order's after that. Two setups sit side by side unless `stack_setups`.
    pub(in crate::root::private_swap) fn render_detail(
        &self,
        operation: ExecutorOperationId,
        stack_setups: bool,
        cx: &Context<'_, Self>,
    ) -> (gpui::Div, Option<gpui::Div>) {
        if let Some(cancelled) = self.cancelled_preparation(operation) {
            let available = self.offers_cancelled_source(operation);
            return render_cancelled_preparation(cancelled, available, cx);
        }
        let Some(record) = self.record(operation) else {
            // An approved setup reserves the stealth account first.
            let reserving = self
                .job
                .as_ref()
                .is_some_and(|job| job.operation == operation && job.kind == SwapJobKind::Setup);
            let error = self
                .tracking
                .get(&operation)
                .and_then(|tracking| tracking.error.clone())
                .or_else(|| self.error.clone());
            return (
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(if reserving {
                        app_muted_shimmer_text("Reserving the swap's stealth account…")
                    } else {
                        app_muted_text(if error.is_some() {
                            "The swap wasn't set up. Nothing was paid."
                        } else {
                            "This swap is no longer saved in this wallet."
                        })
                    })
                    .children(error.map(progress_error)),
                None,
            );
        };
        let pending = self.pending_order(record);
        let stage = self.detail_stage(record);
        let stopped = record.is_swap_setup_stopped();
        let labels = self.labels(record, cx);
        let tracking = self.tracking.get(&operation);
        let job = self
            .job
            .as_ref()
            .filter(|job| job.operation == operation)
            .map(|job| job.kind);
        let cancelling = tracking.is_some_and(|tracking| tracking.cancelling)
            || job == Some(SwapJobKind::Cancel);
        let now = now_unix();
        let offered = offered_status_check(record, stage, now);
        let past_valid_to = swap_valid_to(record).is_some_and(|valid_to| now > valid_to);
        // A stopped setup can't issue more swap work. Its account stays in Stealth accounts.
        let mut actions = if stopped {
            SwapActions::default()
        } else {
            swap_actions(stage, past_valid_to)
        };
        // A setup shown included is past its setup, where the step places the order.
        if let Some(problem) = self
            .setup_retry_problem(record)
            .filter(|_| matches!(stage, SwapStage::SetupPending | SwapStage::SetupSubmitting))
        {
            actions.resume = actions.resume.map(|_| Err(problem));
        }
        // A private Bridge swap whose own account is set up waits for its destination
        // account's setup, which the swap's own setup actions don't cover.
        let destination_setup = swap_private_delivery(record)
            .filter(|_| {
                !stopped
                    && pending.is_none()
                    && matches!(self.stage(record), SwapStage::Ready | SwapStage::Approved)
            })
            .map(|delivery| self.destination_setup_progress(record, delivery))
            .filter(|progress| *progress != SwapSetupProgress::Done);
        // A swap with a saved approval sends a setup again from that setup's own row, which
        // keeps the approval. Without one, the form reviews the swap again.
        let prepared = !stopped && Self::prepared_draft(record).is_some();
        let retried_from_row = prepared
            && matches!(
                stage,
                SwapStage::SetupNotSent | SwapStage::SetupPending | SwapStage::SetupFailed
            );
        if destination_setup.is_some() || retried_from_row {
            actions.resume = None;
        }
        if cancelling {
            actions.cancel = actions
                .cancel
                .map(|_| Err("A cancellation is on its way. Its outcome shows here."));
        }
        // The reported settlement already ran the pre-hook, so a cancellation could only fail
        // or waste its fee. Observation still owns the stage; an unfilled order expires anyway.
        if labels.fill_hint.is_some() {
            actions.cancel = None;
        }
        // Speed up goes with an available Cancel order, as the owner's rule answers for the
        // open order, once that order has been open for a short while.
        let latest_order = record.swap().and_then(|swap| swap.orders().last());
        let mut actions = actions.with_speed_up(
            self.speed_up_refusal(record),
            latest_order.is_some_and(|order| speed_up_shown(order, now)),
        );
        // A cancellation is a recovery, which needs the account's setup confirmed below the
        // open order. Speed up, decided above, stays as the owner's rule answers.
        if !cancelling && latest_order.is_some_and(|order| setup_unresolved_below(record, order)) {
            actions.cancel = actions.cancel.map(|_| Err(CANCEL_WAITS_FOR_SETUP));
        }
        // A new order on the account follows the order retry rules once its setup is confirmed.
        let retry_waits_for_setup =
            latest_order.is_some_and(|order| setup_unresolved_below(record, order));
        if retry_waits_for_setup {
            actions.retry = actions.retry.map(|_| Err(RETRY_WAITS_FOR_SETUP));
        }
        // A second order with a lower minimum, reviewed in the swap form. The open order
        // stays. The action sits in the order step, under the order's ID.
        let busy = self.busy();
        let mut speed_up = actions.speed_up.map(|availability| {
            div()
                .pt(px(4.0))
                .flex()
                .flex_col()
                .items_start()
                .child(
                    app_button("swap-progress-speed-up", "Speed up…")
                        .debug_selector(|| "swap-progress-speed-up".into())
                        .small()
                        .outline()
                        .flex_none()
                        .disabled(busy || availability.is_err())
                        .when_some(availability.err(), gpui_component::button::Button::tooltip)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_speed_up_form(operation, window, cx);
                        })),
                )
                .into_any_element()
        });
        // The automatic attempt to place the approved order waits for the setup's change.
        let waits_for_change = job.is_none()
            && pending.is_none()
            && self.approved_use(record).is_some()
            && tracking.is_some_and(|tracking| tracking.auto_place && tracking.waits_for_change);
        // A swap that set an account up shows the setup, or the order's steps without it. A
        // setup with no account left to show keeps its step among the swap's, alone.
        let sets_up = self.sets_up_account(record);
        let setups = self.setup_columns(record, stage, &labels, cx);
        let in_setup = setups.is_some();
        let steps = swap_steps(stage, &labels)
            .into_iter()
            .enumerate()
            .filter(|(index, _)| {
                if in_setup {
                    *index == 0
                } else {
                    !sets_up || *index > 0
                }
            })
            .map(|(index, mut step)| {
                let detail =
                    if index == 1 && cancelling && step.status == PublicActionStepStatus::Pending {
                        "Cancelling… The order can still fill until the cancellation is final."
                            .into()
                    } else if index == 1 && job == Some(SwapJobKind::Order) {
                        step.status = PublicActionStepStatus::Pending;
                        "Preparing and placing the order…".into()
                    } else if index == 1 && job == Some(SwapJobKind::Requote) {
                        step.status = PublicActionStepStatus::Pending;
                        "Checking the approved terms…".into()
                    } else if index == 1 && waits_for_change {
                        "Waiting for the setup's change to become spendable".into()
                    } else {
                        step.detail.clone()
                    };
                let action = if index == 1 { speed_up.take() } else { None };
                progress_group(
                    &step,
                    detail,
                    format!("swap-step-{}-{index}", operation.opaque_id()),
                    action,
                )
            });
        let error = tracking
            .and_then(|tracking| tracking.shown_error(offered))
            .or_else(|| self.error.clone());
        // A setup's stages show the problem its job ended with.
        let failed_in_steps = setups.as_ref().is_some_and(|setups| {
            setups.iter().any(|setup| {
                self.shown_setup_progress(operation, setup.side)
                    .is_some_and(|progress| {
                        !matches!(progress.end(), None | Some(SetupProgressEnd::Submitted))
                    })
            })
        });
        // A swap with no order yet shows the two steps its reviews named, when it sets an
        // account up: the setup, then the order once every account it uses is ready.
        let placeable = prepared && self.approved_use(record).is_some();
        // A swap that ended at a retired account has no step left to name.
        let review_stepper = prepared_swap_use(record)
            .filter(|_| prepared && stage != SwapStage::SetupRetired)
            .map(|swap_use| {
                let destination = swap_use
                    .approval()
                    .is_some_and(|approval| approval.delivery.private_bridge().is_some())
                    && sets_up_destination(swap_use);
                usize::from(swap_use.is_fresh()) + usize::from(destination)
            })
            .filter(|setups| *setups > 0)
            .map(|setups| {
                render_spend_authorization_steps(&SpendAuthorizationSteps::new(
                    if placeable { 2 } else { 1 },
                    review_steps(setups > 1),
                    swap_steps_hint(setups > 1),
                ))
            });
        // Each setup shown, with whether its row offers to send it again.
        let retries = setups.as_deref().map_or_else(Vec::new, |setups| {
            setups
                .iter()
                .map(|setup| self.setup_retry_availability(record, setup.side, cx))
                .collect()
        });
        let two_setups = retries.len() > 1;
        // The destination setup, shown alone and retried while the swap's own is confirmed.
        let retry_destination = match (setups.as_deref(), retries.as_slice()) {
            (Some([setup]), [Some(Ok(()))]) if setup.side == SetupSide::Destination => labels
                .bridge
                .as_ref()
                .and_then(|bridge| bridge.private.as_ref())
                .map(|private| &private.setups)
                .filter(|[origin, _]| !origin.reused && origin.progress == SwapSetupProgress::Done)
                .map(|[origin, destination]| (&destination.network, &origin.network)),
            _ => None,
        };
        let note: Option<SharedString> = if stopped {
            Some(
                "This setup was stopped, so no order will be placed. Its stealth account stays in Stealth accounts."
                    .into(),
            )
        } else if placeable {
            progress_note(SwapStage::Approved).map(Into::into)
        } else if prepared
            && matches!(stage, SwapStage::SetupSubmitting | SwapStage::SetupPending)
            // A destination setup this session saw fail says what its retry sends instead.
            && !(retry_destination.is_some() && failed_in_steps)
        {
            Some(
                match (tracking.is_some_and(|tracking| tracking.auto_place), two_setups) {
                    (true, false) => {
                        "You can close this. Once the setup is in a block, the wallet checks the quote again and asks you to place the order."
                    }
                    (true, true) => {
                        "You can close this. Once both setups confirm, the wallet checks the quote again and asks you to place the order."
                    }
                    (false, false) => {
                        "The order isn't placed yet. Once the setup is in a block, place it from here."
                    }
                    (false, true) => {
                        "The order isn't placed yet. Once both setups confirm, place it from here."
                    }
                }
                .into(),
            )
        } else if record.is_hidden() && actions.dismiss {
            Some("Removed from the Private tab. Tracking continues in My orders.".into())
        } else if let Some((network, origin)) = retry_destination {
            Some(
                format!(
                    "Retry sends only the setup on {network} again. The fee already paid on {origin} isn't paid again. Nothing was unshielded."
                )
                .into(),
            )
        } else if retried_from_row {
            Some(
                if two_setups {
                    "Retry setup sends that account's setup again with the same stealth account. Nothing was unshielded."
                } else {
                    "Retry setup sends the setup again with the same stealth account. Nothing was unshielded."
                }
                .into(),
            )
        } else if retry_waits_for_setup && actions.retry.is_some() {
            Some(RETRY_WAITS_FOR_SETUP.into())
        } else {
            // A reported fill leaves nothing to do, which the note of its stage doesn't say.
            reported_fill_note(stage, &labels)
                .map(Into::into)
                .or_else(|| progress_note(stage).map(Into::into))
        };
        let latest = pending
            .is_none()
            .then(|| record_swaps(record).pop())
            .flatten();
        let order = record
            .swap()
            .and_then(|swap| swap.orders().last())
            .filter(|_| pending.is_none());
        let checked = tracking
            .and_then(|tracking| tracking.status_checked)
            .filter(|(uid, _)| {
                job.is_none()
                    && error.is_none()
                    && stage.is_observed()
                    && order.is_some()
                    && record_outcome_order(record).is_some_and(|order| order.uid() == *uid)
            })
            .map(|(_, at)| {
                format!(
                    "Checked at {}. The outcome is not confirmed yet. Check again after more blocks arrive.",
                    local_time_label(at),
                )
            });
        let group = swap_order_group(stage, stopped, record.is_hidden());
        let minimum = pending
            .as_ref()
            .map(|pending| (pending.buy, pending.private_minimum))
            .or_else(|| {
                swap_tokens(record)
                    .zip(swap_private_minimum(record))
                    .filter(|_| group == SwapOrderGroup::Open)
                    .map(|((_, buy), minimum)| (buy, minimum))
            });
        let started = pending
            .as_ref()
            .map(|pending| ("Started", pending.started(record)))
            .or_else(|| self.swap_started(record, latest.as_ref(), cx));
        let delivery = pending.map_or_else(|| swap_delivery(record), |pending| pending.delivery);
        // The outcome and an unfilled order's facts are those of the attempt the swap follows.
        let attempts = order.map(|order| SwapAttempts::ending_at(record, order));
        let followed = attempts.map(|attempts| attempts.outcome);
        let outcome =
            followed.and_then(|order| self.render_outcome(record, order, stage, started, cx));
        let not_filled =
            followed.and_then(|order| self.render_not_filled(record, order, stage, cx));
        let facts = match &delivery {
            SwapDelivery::Bridge(bridge) => {
                Some(self.render_bridge_facts(record, order, *bridge, stage, started, cx))
            }
            SwapDelivery::NonEvmBridge(non_evm) => {
                Some(self.render_non_evm_facts(record, order, non_evm, stage, started, cx))
            }
            _ if outcome.is_none() => self.render_facts(order, &delivery, minimum, started, cx),
            _ => None,
        };
        let steps_shown = shows_steps(stage, outcome.is_some());
        // The accounts no setup above names stay among the facts.
        let named = setups.as_ref().is_none_or(|setups| !setups.is_empty());
        let accounts = if steps_shown && sets_up && named {
            self.account_facts(record, &labels, stage, cx)
        } else {
            Vec::new()
        };
        let facts = if accounts.is_empty() {
            facts
        } else {
            Some(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(accounts)
                    .children(facts),
            )
        };
        let stepper = steps_shown.then(|| match setups.as_deref() {
            None | Some([]) => render_submission_progress_groups(steps),
            Some(setups) => self.render_setups(operation, setups, &retries, stack_setups, cx),
        });
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .children(review_stepper)
            .children(stepper)
            .children(not_filled)
            .children(facts)
            .children(outcome)
            .children(attempts.and_then(|attempts| self.render_attempts(record, &attempts, cx)))
            .children(
                attempts
                    .and_then(|attempts| earlier_attempts_note(&attempts))
                    .map(|note| app_muted_text(note).whitespace_normal()),
            )
            .children(note.map(|note| app_muted_text(note).whitespace_normal()))
            .children(checked.map(|message| app_muted_text(message).whitespace_normal()))
            .children(error.map(progress_error));
        (
            body,
            Some(self.render_progress_actions(operation, stage, offered, actions, job, cx)),
        )
    }

    /// Whether the swap use `record` shows sets a stealth account up: its own, which the use
    /// reserved fresh, or a private Bridge swap's destination account. A record without a use
    /// is a fresh reservation, unless a draft reuses its account.
    fn sets_up_account(&self, record: &ExecutorRecord) -> bool {
        let swap_use = self
            .shown_swap(record)
            .and_then(|swap| record.swap_use(swap.swap_use));
        match swap_use {
            Some(swap_use) => {
                let destination = swap_use
                    .approval()
                    .is_some_and(|approval| approval.delivery.private_bridge().is_some())
                    && sets_up_destination(swap_use);
                swap_use.is_fresh() || destination
            }
            None => self
                .pending_order(record)
                .is_none_or(|pending| !pending.reuse_account),
        }
    }

    /// The setups the detail of `record` shows in place of its order's steps, the swap's own
    /// account first: one for each account the swap sets up whose setup isn't confirmed. `None`
    /// once the swap shows its order, as a swap that sets no account up does from the start. A
    /// swap that ended at its setup keeps showing it.
    fn setup_columns(
        &self,
        record: &ExecutorRecord,
        stage: SwapStage,
        labels: &SwapLabels,
        cx: &App,
    ) -> Option<Vec<SetupColumn>> {
        let in_setup = matches!(
            stage,
            SwapStage::SetupNotSent
                | SwapStage::SetupSubmitting
                | SwapStage::SetupPending
                | SwapStage::SetupFailed
                | SwapStage::SetupRetired
        ) || (record.is_swap_setup_stopped()
            && matches!(stage, SwapStage::Ready | SwapStage::Approved));
        if !in_setup || !self.sets_up_account(record) {
            return None;
        }
        let setup = swap_steps(stage, labels).into_iter().next()?;
        let private = labels
            .bridge
            .as_ref()
            .and_then(|bridge| bridge.private.as_ref());
        let Some(private) = private else {
            return Some(vec![SetupColumn {
                side: SetupSide::Origin,
                heading: None,
                progress: swap_setup_progress(stage),
                confirmation: self.setup_confirmation_detail(record, cx),
                step: setup,
            }]);
        };
        let columns = [SetupSide::Origin, SetupSide::Destination]
            .into_iter()
            .zip(&private.setups)
            .zip(&setup.children)
            .filter(|((_, account), _)| {
                !account.reused && account.progress != SwapSetupProgress::Done
            })
            .map(|((side, account), child)| {
                let mut step = child.clone();
                // A retired account keeps the reason its swap can't continue.
                if side == SetupSide::Origin && stage == SwapStage::SetupRetired {
                    step.detail.clone_from(&setup.detail);
                }
                SetupColumn {
                    side,
                    heading: Some((account.network.clone(), account.account)),
                    progress: account.progress,
                    confirmation: match side {
                        SetupSide::Origin => self.setup_confirmation_detail(record, cx),
                        SetupSide::Destination => account.detail.clone(),
                    },
                    step,
                }
            })
            .collect();
        Some(columns)
    }

    /// Whether the open detail shows two setups, which sit side by side in a wider dialog.
    pub(in crate::root::private_swap) fn shows_two_setups(&self, cx: &App) -> bool {
        let Some(SwapDialogView::Detail(operation)) =
            self.dialog.as_ref().map(|dialog| dialog.view)
        else {
            return false;
        };
        if self.cancelled_preparation(operation).is_some() {
            return false;
        }
        self.record(operation)
            .and_then(|record| {
                let stage = self.detail_stage(record);
                self.setup_columns(record, stage, &self.labels(record, cx), cx)
            })
            .is_some_and(|setups| setups.len() > 1)
    }

    /// The stages this session holds of `side`'s setup of the swap `operation`, when they
    /// tell how it went: its job ended, or still runs. A job that stopped without an end
    /// leaves stages that say nothing.
    pub(super) fn shown_setup_progress(
        &self,
        operation: ExecutorOperationId,
        side: SetupSide,
    ) -> Option<&SetupProgress> {
        self.tracking
            .get(&operation)?
            .setup_progress(side)
            .filter(|progress| progress.end().is_some() || self.submitting_setup(operation))
    }

    /// Whether the detail of the swap `operation` shows those stages of `side`'s setup: the
    /// swap is saved, and its detail shows that account's setup. A result the stages don't
    /// show is told as the swap's error instead.
    pub(in crate::root::private_swap) fn shows_setup_progress(
        &self,
        operation: ExecutorOperationId,
        side: SetupSide,
        cx: &App,
    ) -> bool {
        if self.shown_setup_progress(operation, side).is_none()
            || self.cancelled_preparation(operation).is_some()
        {
            return false;
        }
        self.record(operation)
            .and_then(|record| {
                let stage = self.detail_stage(record);
                self.setup_columns(record, stage, &self.labels(record, cx), cx)
            })
            .is_some_and(|setups| setups.iter().any(|setup| setup.side == side))
    }
}
