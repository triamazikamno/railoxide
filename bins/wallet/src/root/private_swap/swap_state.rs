#[allow(clippy::wildcard_imports)]
use super::*;

impl PrivateSwapsView {
    /// Why `record`'s latest order can't be sped up, as the owner's rule answered when the
    /// records were last read. `None` offers the speed-up. Signing checks again.
    pub(super) fn speed_up_refusal(&self, record: &ExecutorRecord) -> Option<SwapSpeedUpRefusal> {
        // Debug UI fixture: the owner holds no synthesized record to ask about.
        #[cfg(debug_assertions)]
        if let Some(refusal) = ui_fixture::speed_up_refusal() {
            return refusal.err();
        }
        let order = record.swap().and_then(|swap| swap.orders().last());
        match (order, self.speed_up_refusals.get(&record.operation())) {
            (Some(order), Some((uid, refusal))) if order.uid() == *uid => *refusal,
            _ => Some(SwapSpeedUpRefusal::Unavailable),
        }
    }

    /// Render again when the Speed up delay of the open detail's latest order ends, so the
    /// action appears by itself. Nothing waits once the delay has passed or another view shows.
    pub(super) fn schedule_speed_up_reveal(&mut self, cx: &Context<'_, Self>) {
        let now = now_unix();
        let shown_at = self
            .dialog
            .as_ref()
            .and_then(|dialog| match dialog.view {
                dialog::SwapDialogView::Detail(operation) => self.record(operation),
                _ => None,
            })
            .and_then(|record| record.swap()?.orders().last())
            .and_then(model::speed_up_shown_at)
            .filter(|shown_at| *shown_at > now);
        let Some(shown_at) = shown_at else {
            self.speed_up_reveal = None;
            return;
        };
        if self
            .speed_up_reveal
            .as_ref()
            .is_some_and(|(scheduled, _)| *scheduled == shown_at)
        {
            return;
        }
        let wait = Duration::from_secs(shown_at - now);
        let reveal = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |this, cx| {
                this.speed_up_reveal = None;
                cx.notify();
            });
        });
        self.speed_up_reveal = Some((shown_at, reveal));
    }

    /// The attribution evidence read with `record`, an account of any loaded network.
    pub(in crate::root) fn attribution(
        &self,
        record: &ExecutorRecord,
    ) -> Option<&wallet_ops::ExecutorAttribution> {
        self.attributions.get(&record.operation())
    }

    pub(super) fn stage(&self, record: &ExecutorRecord) -> SwapStage {
        let operation = record.operation();
        let submitting = self
            .job
            .as_ref()
            .is_some_and(|job| job.operation == operation && job.kind == SwapJobKind::Setup);
        let setup = self
            .tracking
            .get(&operation)
            .and_then(|tracking| tracking.setup);
        account_stage(
            record,
            self.origin_chain_id,
            setup,
            submitting,
            self.attribution(record),
        )
    }

    /// This session's last read at the latest block that showed `record`'s own setup
    /// included, while the record still leaves that setup unresolved. It is a hint for
    /// placing the order: signing reads the account again.
    pub(in crate::root) fn setup_inclusion(
        &self,
        record: &ExecutorRecord,
    ) -> Option<SwapSetupInclusion> {
        wallet_ops::own_pending_swap_setup(record)?;
        self.tracking.get(&record.operation())?.inclusion
    }

    /// Whether private sync or this session's latest read shows `record`'s own setup, which
    /// the record leaves unresolved, included.
    pub(in crate::root) fn own_setup_included(&self, record: &ExecutorRecord) -> bool {
        if wallet_ops::own_pending_swap_setup(record).is_none() {
            return false;
        }
        self.setup_inclusion(record).is_some()
            || matches!(
                self.setup_confirmation(record),
                Some(model::SwapSetupConfirmation::Included(_))
            )
    }

    /// A new submission must not borrow the previous order's outcome. Once its own swap use
    /// records an order, that record becomes the source of progress even if submission is
    /// still running.
    ///
    /// Without a draft of this session, a reused account's prepared swap gives one from its
    /// saved approval: see [`PendingSwapOrder::prepared`].
    ///
    /// A speed-up's draft counts only while its order is being placed. Its open order is still
    /// live, so a speed-up that ends without an order shows that order again.
    pub(super) fn pending_order(&self, record: &ExecutorRecord) -> Option<PendingSwapOrder> {
        let operation = record.operation();
        self.tracking
            .get(&operation)
            .and_then(|tracking| tracking.pending_order.clone())
            .filter(|pending| {
                pending.previous_order
                    == model::swap_use_last_order(record, pending.swap_use)
                        .map(SwapOrderRecord::uid)
            })
            .filter(|pending| {
                !pending.speeds_up
                    || self.job.as_ref().is_some_and(|job| {
                        job.operation == operation && job.kind == SwapJobKind::Order
                    })
            })
            .or_else(|| PendingSwapOrder::prepared(record))
            .map(|mut pending| {
                if let SwapDelivery::Bridge(delivery) = &mut pending.delivery
                    && delivery.is_private()
                    && delivery.receiver.is_zero()
                    && let Some(SwapDelivery::Bridge(bound)) = model::prepared_swap_use(record)
                        .filter(|swap_use| swap_use.id() == pending.swap_use)
                        .and_then(SwapUseRecord::approval)
                        .map(|approval| approval.delivery.clone())
                    && bound.is_private()
                    && bound.destination_chain == delivery.destination_chain
                    && !bound.receiver.is_zero()
                {
                    // Preparation binds the new account after this session saved its draft.
                    // Keep the draft's terms, resolving only its placeholder receiver.
                    delivery.receiver = bound.receiver;
                }
                pending
            })
    }

    /// The swap use of `record` whose first order the approval saved with it still binds, once
    /// every account the swap uses is ready for that order: the swap's own account the use set
    /// up is confirmed, or this session's latest read shows its setup included, a destination
    /// account the use set up is confirmed, and an account it reuses is set up already. The
    /// approved order is then placed without another review, whichever of its accounts needed
    /// setup. Signing still checks each account afresh.
    pub(super) fn approved_use<'a>(
        &self,
        record: &'a ExecutorRecord,
    ) -> Option<(SwapUseId, &'a SwapApproval)> {
        if record.is_swap_setup_stopped() {
            return None;
        }
        let swap_use = model::prepared_swap_use(record)?;
        let approval = swap_use.approval()?;
        let operation = record.operation();
        let source_ready = if swap_use.is_fresh() {
            self.stage(record) == SwapStage::Approved || self.setup_inclusion(record).is_some()
        } else {
            account_setup_progress(
                record,
                self.origin_chain_id,
                self.tracking
                    .get(&operation)
                    .and_then(|tracking| tracking.setup),
                false,
            ) == SwapSetupProgress::Done
        };
        let destination_ready = match approval.delivery.private_bridge() {
            Some(delivery) => {
                let swap = SwapIdentity {
                    operation,
                    swap_use: swap_use.id(),
                };
                self.swap_destination_setup_progress(swap, delivery) == SwapSetupProgress::Done
            }
            None => true,
        };
        (source_ready && destination_ready).then_some((swap_use.id(), approval))
    }

    /// The private Bridge delivery `record`'s own progress is about: its draft's, or its
    /// latest order's or approved one.
    pub(super) fn shown_private_delivery(&self, record: &ExecutorRecord) -> Option<BridgeDelivery> {
        match self.pending_order(record) {
            Some(pending) => pending.delivery.private_bridge(),
            None => swap_private_delivery(record),
        }
    }

    /// Presentation only; signing and observation still use the durable account stage. A
    /// private Bridge swap also shows its destination stealth account's setup, and a recovery
    /// of held proceeds on the destination network.
    pub(super) fn progress_stage(&self, record: &ExecutorRecord) -> SwapStage {
        if let Some(pending) = self.pending_order(record) {
            // A draft's own account is set up. A destination account its swap use sets up
            // shows that setup until it is confirmed; one it reuses has none to wait for.
            let setup = record
                .swap_use(pending.swap_use)
                .filter(|swap_use| sets_up_destination(swap_use))
                .and_then(|_| model::swap_use_destination(record, pending.swap_use));
            return match setup {
                Some((delivery, _)) => model::private_bridge_setup_stage(
                    SwapStage::Ready,
                    self.swap_destination_setup_progress(
                        SwapIdentity {
                            operation: record.operation(),
                            swap_use: pending.swap_use,
                        },
                        delivery,
                    ),
                ),
                None => SwapStage::Ready,
            };
        }
        let stage = self.stage(record);
        let Some(delivery) = swap_private_delivery(record) else {
            return stage;
        };
        match stage {
            SwapStage::Ready | SwapStage::Approved => model::private_bridge_setup_stage(
                stage,
                self.destination_setup_progress(record, delivery),
            ),
            SwapStage::Order(SwapOrderState::HeldOnDestination)
                if self.held_proceeds_recovered(record, delivery) =>
            {
                SwapStage::Recovered
            }
            _ => stage,
        }
    }

    /// [`Self::progress_stage`] as the swap's detail shows it. Before its order, a swap whose
    /// own setup this session's latest read shows included is past that setup, as once the
    /// setup is confirmed, while its record still leaves the setup unresolved.
    pub(in crate::root) fn detail_stage(&self, record: &ExecutorRecord) -> SwapStage {
        let stage = self.progress_stage(record);
        if stage != SwapStage::SetupPending || self.setup_inclusion(record).is_none() {
            return stage;
        }
        let ready = if record.swap_approval().is_some() && record.active_swap_use().is_some() {
            SwapStage::Approved
        } else {
            SwapStage::Ready
        };
        match swap_private_delivery(record) {
            Some(delivery) => model::private_bridge_setup_stage(
                ready,
                self.destination_setup_progress(record, delivery),
            ),
            None => ready,
        }
    }

    pub(super) const fn busy(&self) -> bool {
        self.job.is_some()
            || self.pending_authorization.is_some()
            || self.public_authorization.is_some()
            || self.public_job.is_some()
    }

    /// The confirmed block of the session's last synced head.
    pub(super) fn confirmed_block(&self, cx: &gpui::App) -> Option<u64> {
        self.root
            .upgrade()?
            .read(cx)
            .confirmed_block(self.origin_chain_id)
    }

    pub(super) fn setup_confirmation(
        &self,
        record: &ExecutorRecord,
    ) -> Option<model::SwapSetupConfirmation> {
        // Debug UI fixture: no private note shows the setup it holds included.
        #[cfg(debug_assertions)]
        if let Some((block, _)) = ui_fixture::setup_included() {
            return Some(model::SwapSetupConfirmation::Included(block));
        }
        model::swap_setup_confirmation(
            record,
            &self
                .private_session()?
                .observation_rx
                .borrow()
                .snapshot
                .utxos,
        )
    }

    /// Why the swap's own setup, before its order, isn't sent again: private sync shows it
    /// included or resolved, or this session's latest read shows it included.
    pub(super) fn setup_retry_problem(&self, record: &ExecutorRecord) -> Option<&'static str> {
        (matches!(
            self.stage(record),
            SwapStage::SetupPending | SwapStage::SetupSubmitting
        ) && (self.setup_confirmation(record).is_some() || self.setup_inclusion(record).is_some()))
        .then_some(SETUP_ALREADY_INCLUDED)
    }

    /// [`Self::setup_retry_problem`] for the destination stealth account of the private Bridge
    /// swap `swap`, whose setup is sent and unconfirmed: its record and the private notes of
    /// its own network's session tell that the setup is included.
    fn destination_setup_retry_problem(
        &self,
        swap: SwapIdentity,
        delivery: BridgeDelivery,
        cx: &gpui::App,
    ) -> Option<&'static str> {
        let destination = self.claimed_destination_account(swap, delivery)?;
        let (session, _) = self.destination_owner(delivery.destination_chain, cx)?;
        let observation = session.observation_rx.borrow();
        model::swap_setup_confirmation(destination, &observation.snapshot.utxos)
            .map(|_| SETUP_ALREADY_INCLUDED)
    }

    /// How far `side`'s setup of the prepared swap `record` shows is, apart from the other
    /// account's. `None` for an account the swap doesn't set up: one it reuses, or the
    /// destination of a swap that has none.
    pub(super) fn prepared_setup_progress(
        &self,
        record: &ExecutorRecord,
        side: SetupSide,
    ) -> Option<SwapSetupProgress> {
        let swap_use = model::prepared_swap_use(record)?;
        let operation = record.operation();
        match side {
            SetupSide::Origin => swap_use.is_fresh().then(|| {
                account_setup_progress(
                    record,
                    self.origin_chain_id,
                    self.tracking
                        .get(&operation)
                        .and_then(|tracking| tracking.setup),
                    self.submitting_setup(operation),
                )
            }),
            SetupSide::Destination => {
                let (delivery, _) = model::swap_use_destination(record, swap_use.id())?;
                sets_up_destination(swap_use).then(|| {
                    self.swap_destination_setup_progress(
                        SwapIdentity {
                            operation,
                            swap_use: swap_use.id(),
                        },
                        delivery,
                    )
                })
            }
        }
    }

    /// Whether `side`'s setup of the swap `record` shows can be sent again by itself, which
    /// keeps the approval saved with the swap: while it isn't sent, is sent and unconfirmed,
    /// or failed. A setup seen included waits for its confirmation, with the reason. `None`
    /// while the setup's job runs, once it is confirmed, for a stopped or retired setup, and
    /// for a swap without a saved approval, which its form reviews instead.
    ///
    /// A swap's approval is saved only for an own account that has an address. So while a swap
    /// that sets its own account up has none for it, the destination setup waits for the swap's
    /// own, with the reason. Only the missing address holds it back, never how far the swap's
    /// own setup is.
    ///
    /// The swap's own setup that an order was placed on while it was included is sent again
    /// while the record leaves it unresolved, with the order open or ended. Only private sync
    /// or this session's latest read showing it included again disables that, with the reason.
    pub(super) fn setup_retry_availability(
        &self,
        record: &ExecutorRecord,
        side: SetupSide,
        cx: &gpui::App,
    ) -> SetupRetryAvailability {
        if record.is_swap_setup_stopped() {
            return None;
        }
        if side == SetupSide::Origin && model::order_setup_use(record).is_some() {
            if record.is_retired() || self.submitting_setup(record.operation()) {
                return None;
            }
            return Some(if self.own_setup_included(record) {
                Err(SETUP_ALREADY_INCLUDED.into())
            } else {
                Ok(())
            });
        }
        let swap_use =
            model::prepared_swap_use(record).filter(|swap_use| swap_use.approval().is_some())?;
        let progress = self.prepared_setup_progress(record, side)?;
        let (retired, included) = match side {
            SetupSide::Origin => (record.is_retired(), self.setup_retry_problem(record)),
            SetupSide::Destination => {
                let swap = SwapIdentity {
                    operation: record.operation(),
                    swap_use: swap_use.id(),
                };
                let (delivery, _) = model::swap_use_destination(record, swap_use.id())?;
                (
                    self.claimed_destination_account(swap, delivery)
                        .is_some_and(ExecutorRecord::is_retired),
                    self.destination_setup_retry_problem(swap, delivery, cx),
                )
            }
        };
        if retired {
            return None;
        }
        let underived = swap_use.is_fresh() && record.address().is_none();
        let waits_for_origin = (side == SetupSide::Destination && underived).then(|| {
            gpui::SharedString::from(format!(
                "Set up the account on {} first.",
                form::network_name(self.origin_chain_id)
            ))
        });
        // A setup that waits for a broadcaster's answer for long is offered again, which stops
        // the wait of its job first.
        let outlasted = progress == SwapSetupProgress::Submitting
            && self.setup_wait_outlasted(record.operation(), side);
        match progress {
            SwapSetupProgress::NotSent | SwapSetupProgress::Failed => {
                Some(waits_for_origin.map_or(Ok(()), Err))
            }
            SwapSetupProgress::Submitting if outlasted => Some(
                waits_for_origin
                    .or_else(|| included.map(Into::into))
                    .map_or(Ok(()), Err),
            ),
            SwapSetupProgress::Pending => Some(
                waits_for_origin
                    .or_else(|| included.map(Into::into))
                    .map_or(Ok(()), Err),
            ),
            SwapSetupProgress::NetworkLoading
            | SwapSetupProgress::Submitting
            | SwapSetupProgress::Done => None,
        }
    }
}
