use super::*;

impl PrivateSwapsView {
    /// The order `record`'s swap still tracks that the orderbook reports filled, with that
    /// report and what the reported trade block gave early.
    pub(super) fn reported_order<'a>(
        &'a self,
        record: &'a ExecutorRecord,
    ) -> Option<(
        &'a SwapOrderRecord,
        &'a SwapOrderHint,
        Option<&'a SwapEarlyHint>,
    )> {
        let tracking = self.tracking.get(&record.operation())?;
        model::tracked_orders(record).into_iter().find_map(|order| {
            let hint = tracking
                .order_hints
                .get(&order.uid())
                .filter(|hint| hint.report.status == CowOrderStatusHint::Fulfilled)?;
            let early = tracking
                .early_hints
                .get(&order.uid())
                .filter(|early| early.trade_block == hint.trade_block);
            Some((order, hint, early))
        })
    }

    /// The orderbook's report that an order the swap still tracks filled, while canonical
    /// observation still shows the swap open, with what the reported trade block showed of
    /// the settlement once it was read.
    pub(super) fn fill_hint(
        &self,
        record: &ExecutorRecord,
        cx: &gpui::App,
    ) -> Option<SwapFillHint> {
        if !matches!(
            self.stage(record),
            SwapStage::Order(SwapOrderState::Open | SwapOrderState::PreHookOnly { expired: false })
        ) {
            return None;
        }
        let (order, hint, early) = self.reported_order(record)?;
        let buy = swap_tokens(record).map(|(_, buy)| buy);
        let settlement = early.and_then(|early| early.settlement);
        let root = self.root.upgrade()?;
        let root = root.read(cx);
        let depth = root
            .effective_chain_configs
            .get(self.origin_chain_id)?
            .finality_depth;
        let head = root
            .chain_states
            .get(&self.origin_chain_id)
            .and_then(crate::root::chain_load::ChainUtxoState::sync_tip)
            .and_then(|tip| tip.head_block);
        // Debug UI fixture: its reported fill counts from a head of its own.
        #[cfg(debug_assertions)]
        let head = ui_fixture::reported_head().or(head);
        let shield = match (order.delivery(), buy) {
            (SwapDelivery::Reshield, Some(buy)) => SwapEarlyHint::seen(early, |settlement| {
                settlement
                    .shielded
                    .map(|amount| self.token_amount(buy, amount, cx))
            }),
            _ => SwapSeen::Looking,
        };
        Some(SwapFillHint {
            confirmations: hint
                .trade_block
                .zip(head)
                .map(|(block, head)| head.saturating_sub(block)),
            depth,
            // The amount in the read block comes before the orderbook's.
            bought: settlement
                .map(|settlement| settlement.bought)
                .or(hint.report.executed_buy_amount)
                .zip(buy)
                .map(|(amount, buy)| self.token_amount(buy, amount, cx)),
            detected: settlement.is_some(),
            shield_transaction: settlement
                .and_then(|settlement| settlement.shield_transaction)
                .filter(|_| matches!(shield, SwapSeen::Detected(_))),
            shield,
            origin: form::network_name(self.origin_chain_id),
        })
    }

    /// The swaps the Private tab shows, newest first.
    pub(super) fn shown_swaps(&self) -> impl Iterator<Item = (&ExecutorRecord, SwapStage)> {
        self.records
            .iter()
            .filter(|record| is_swap_record(record) || self.pending_order(record).is_some())
            .filter_map(|record| {
                let stage = self.progress_stage(record);
                // A reused account whose later swap was cancelled too holds only cancelled
                // preparations, which My orders lists.
                (!record.is_swap_setup_stopped()
                    && (self.pending_order(record).is_some()
                        || !model::holds_only_cancelled_preparations(record)
                            && stage.is_shown_on_private_tab(
                                record.is_hidden(),
                                model::swap_valid_to(record)
                                    .is_some_and(|valid_to| valid_to < now_unix()),
                            )))
                .then_some((record, stage))
            })
    }

    pub(super) fn next_observations(
        &self,
        cx: &gpui::App,
    ) -> Option<(
        tokio::runtime::Handle,
        Arc<ExecutorOwner>,
        Vec<ObservationPage>,
    )> {
        // Debug UI fixture: a synthesized record has nothing to observe.
        if ui_fixture::holds_swaps() || self.session.is_none() || !self.session_is_current(cx) {
            return None;
        }
        let confirmed = self.confirmed_block(cx)?;
        let http = self.http(cx)?;
        let busy = self.job.as_ref().map(|job| job.operation);
        let pages = self
            .records
            .iter()
            .filter(|record| Some(record.operation()) != busy)
            // A stopped setup never places its order, so nothing waits on its observation. Its
            // notes stay reserved until the user checks or releases them as locked notes.
            .filter(|record| !record.is_swap_setup_stopped())
            // Only unfinished work is observed automatically. Ended swaps retain their
            // confirmed history across restart, even when hidden or lacking old evidence.
            // An explicit retry, recovery or lock check refreshes that one account.
            .filter(|record| self.stage(record).is_observed())
            .flat_map(|record| {
                let tracking = self.tracking.get(&record.operation());
                let orders = model::tracked_orders(record);
                if !orders.is_empty() && !tracking.is_some_and(|tracking| tracking.cancelling) {
                    // Each order the swap still tracks gets its own page once its settlement
                    // is located, from its own report alone. Competing orders are both
                    // tracked until one trade is verified, and then only the traded one.
                    return orders
                        .into_iter()
                        .filter_map(|order| {
                            // Private sync's spend of the pre-hook inputs locates a
                            // settlement the orderbook hasn't reported. It comes last:
                            // anyone can run the public pre-hook alone, and that block
                            // would never hold the trade.
                            let block = tracking
                                .and_then(|tracking| tracking.order_hints.get(&order.uid()))
                                .and_then(|hint| hint.trade_block)
                                .or_else(|| {
                                    order.observations().traded.map(|trade| trade.block.number)
                                })
                                .or_else(|| {
                                    self.private_owner()?
                                        .synced_settlement_block(record, order.uid())
                                })?;
                            (block <= confirmed).then_some(ObservationPage {
                                operation: record.operation(),
                                settlement: Some((order.uid(), block)),
                                // Settlement checks do not page through account history or
                                // catch up.
                                range: Some(confirmed..confirmed.saturating_add(1)),
                                destination: None,
                                route: self.swap_route(&http, record.operation()),
                            })
                        })
                        .collect::<Vec<_>>();
                }
                // A setup waits on one read of its account at the confirmed block each pass.
                if record.swap().is_none() {
                    return has_issued_setup(record)
                        .then_some(ObservationPage {
                            operation: record.operation(),
                            settlement: None,
                            range: None,
                            destination: None,
                            route: self.swap_route(&http, record.operation()),
                        })
                        .into_iter()
                        .collect();
                }
                tracking
                    .and_then(|tracking| tracking.cursor)
                    .or_else(|| swap_observation_start(record))
                    .map(|cursor| ObservationPage {
                        operation: record.operation(),
                        settlement: None,
                        range: Some(swap_observation_range(cursor, confirmed)),
                        destination: None,
                        route: self.swap_route(&http, record.operation()),
                    })
                    .into_iter()
                    .collect()
            })
            // An order placed on the swap's own included setup leaves that setup unresolved in
            // the record. Its account is read each pass beside the order's pages, while the
            // order is open and after it ended: at the confirmed block, which can resolve the
            // setup, and at the latest block, which shows whether it is still included.
            .chain(
                self.records
                    .iter()
                    .filter(|record| Some(record.operation()) != busy)
                    .filter(|record| {
                        record.swap().is_some()
                            && wallet_ops::own_pending_swap_setup(record).is_some()
                    })
                    .map(|record| ObservationPage {
                        operation: record.operation(),
                        settlement: None,
                        range: None,
                        destination: None,
                        route: self.swap_route(&http, record.operation()),
                    }),
            )
            .chain(
                self.records
                    .iter()
                    .filter(|record| Some(record.operation()) != busy)
                    .filter_map(|record| self.destination_observation(record, &http, cx)),
            )
            .collect::<Vec<_>>();
        let owner = self.private_owner().filter(|_| !pages.is_empty())?;
        Some((self.runtime.clone(), Arc::clone(owner), pages))
    }

    /// The next observation of a private Bridge swap's destination stealth account, on its own
    /// network and through that network's owner, while its setup is unconfirmed and the swap
    /// has no order. Each pass reads the account once, like the swap's own setup. A reused
    /// account's draft is such a swap while its swap use has no order, whatever orders the
    /// account's earlier swaps have.
    fn destination_observation(
        &self,
        record: &ExecutorRecord,
        http: &wallet_ops::HttpContext,
        cx: &gpui::App,
    ) -> Option<ObservationPage> {
        if record.is_swap_setup_stopped() {
            return None;
        }
        let (delivery, destination) = match self.pending_order(record) {
            Some(pending) if pending.reuse_account => {
                let swap = SwapIdentity {
                    operation: record.operation(),
                    swap_use: pending.swap_use,
                };
                let (delivery, _) = model::swap_use_destination(record, pending.swap_use)?;
                (delivery, self.swap_destination_account(swap, delivery)?)
            }
            None if model::holds_only_cancelled_preparations(record) => return None,
            _ if record.swap().is_some() || record.is_retired() => return None,
            _ => {
                let delivery = swap_private_delivery(record)?;
                (delivery, self.destination_account(record, delivery)?)
            }
        };
        let chain_id = delivery.destination_chain;
        let tracking = self.tracking.get(&record.operation());
        let observed = tracking.and_then(|tracking| tracking.destination_setup);
        if account_stage(
            destination,
            chain_id,
            observed,
            false,
            self.attribution(destination),
        ) != SwapStage::SetupPending
        {
            return None;
        }
        let (_, owner) = self.destination_owner(chain_id, cx)?;
        // Nothing is read before the destination network has a confirmed block.
        self.root.upgrade()?.read(cx).confirmed_block(chain_id)?;
        has_issued_setup(destination).then_some(ObservationPage {
            operation: destination.operation(),
            settlement: None,
            range: None,
            destination: Some(DestinationPage {
                owner,
                origin: record.operation(),
            }),
            // A private Bridge swap's two accounts share its route.
            route: self.swap_route(http, record.operation()),
        })
    }

    /// Orders to ask the orderbook about in this pass, at most one report each. An open order
    /// is asked about until its settlement is verified, including fills while offline. Two
    /// orders that compete for one trade are both asked about, each through the swap's own
    /// route, until one trade is verified; after that only the traded one is.
    /// These reports only locate evidence; they cannot change a persisted outcome. A traded
    /// order's executed fee is asked for once per session until it's recorded, also after
    /// verification ends the reports.
    ///
    /// Handed-off Bridge orders ask their provider through the same route until an outcome is
    /// recorded, including after a restart, backing off while it reports none. A destination
    /// chain that isn't enabled here isn't tracked.
    ///
    /// An order whose fill is reported, and whose settlement isn't recorded yet, gets at most
    /// one early request with the same back-off, as [`early_request`] describes: its reported
    /// block is read once, and a Bridge order's provider is asked. A private Bridge order's
    /// request carries its destination chain, when that is enabled here, so its fill block is
    /// read there. Its answers are kept for display in this session and nothing of them is
    /// persisted. A Bridge order is asked about early or as handed off, never both.
    pub(super) fn next_order_hints(
        &self,
        cx: &gpui::App,
    ) -> Option<(tokio::runtime::Handle, Arc<ExecutorOwner>, Vec<HintRequest>)> {
        // Debug UI fixture: no provider is asked about a synthesized order.
        if ui_fixture::holds_swaps() || self.session.is_none() || !self.session_is_current(cx) {
            return None;
        }
        let http = self.http(cx)?;
        let root = self.root.upgrade();
        let chains = root
            .as_ref()
            .map(|root| &root.read(cx).effective_chain_configs);
        let busy = self.job.as_ref().map(|job| job.operation);
        let now = Instant::now();
        let requests = self
            .records
            .iter()
            .filter(|record| Some(record.operation()) != busy)
            .flat_map(|record| {
                let orders = model::tracked_orders(record);
                let tracking = self.tracking.get(&record.operation());
                // Until verification succeeds, keep refreshing the untrusted location,
                // including after a reorg moves a reported trade to a different block.
                let stage = self.stage(record);
                let wanted = matches!(
                    stage,
                    SwapStage::SubmissionPending
                        | SwapStage::SubmissionRejected
                        | SwapStage::Order(
                            SwapOrderState::Open
                                | SwapOrderState::PreHookOnly { expired: false }
                                | SwapOrderState::Traded,
                        )
                );
                let mut bridges = record
                    .swap_bridges_to_track()
                    .filter(|order| {
                        tracking
                            .and_then(|tracking| {
                                // A poll that backed off before the hand-off was recorded
                                // doesn't delay tracking it.
                                tracking
                                    .bridge_polls
                                    .get(&order.uid())
                                    .filter(|_| !tracking.early_hints.contains_key(&order.uid()))
                            })
                            .is_none_or(|poll| poll.next_at <= now)
                    })
                    .filter_map(|order| {
                        let delivery = match order.delivery() {
                            SwapDelivery::Bridge(delivery) => delivery,
                            // A non-EVM network has no chain to read.
                            SwapDelivery::NonEvmBridge(_) => return Some((order.uid(), None)),
                            SwapDelivery::Reshield | SwapDelivery::External { .. } => {
                                return None;
                            }
                        };
                        let chain = chains?
                            .get(delivery.destination_chain)
                            .filter(|chain| chain.enabled)?;
                        Some((order.uid(), Some(chain.clone())))
                    })
                    .collect::<Vec<_>>();
                // Handed-off orders are polled once per pass, with the first request.
                orders
                    .into_iter()
                    .filter_map(|order| {
                        // A verified trade ends routine hints, but its fee is asked for once.
                        let fee = model::needs_executed_fee(order)
                            && tracking
                                .is_none_or(|tracking| !tracking.fee_asked.contains(&order.uid()));
                        (wanted || fee || !bridges.is_empty()).then(|| {
                            let destination = match order.delivery() {
                                SwapDelivery::Bridge(delivery) if delivery.is_private() => chains
                                    .and_then(|chains| chains.get(delivery.destination_chain))
                                    .filter(|chain| chain.enabled),
                                _ => None,
                            };
                            let early = tracking.and_then(|tracking| {
                                early_request(stage, order, tracking, destination.is_some(), now)
                            });
                            HintRequest {
                                operation: record.operation(),
                                uid: order.uid(),
                                hint: wanted,
                                fee,
                                route: self.swap_route(&http, record.operation()),
                                bridges: std::mem::take(&mut bridges),
                                early,
                                destination: early
                                    .and(destination)
                                    .map(|chain| Box::new(chain.clone())),
                            }
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let owner = self.private_owner().filter(|_| !requests.is_empty())?;
        Some((self.runtime.clone(), Arc::clone(owner), requests))
    }

    /// The orders whose reported settlement is read early, as [`early_order`] admits them.
    fn early_orders(&self) -> HashSet<OrderUid> {
        self.records
            .iter()
            .flat_map(|record| {
                let stage = self.stage(record);
                let hints = self
                    .tracking
                    .get(&record.operation())
                    .map(|tracking| &tracking.order_hints);
                model::tracked_orders(record)
                    .into_iter()
                    .filter(move |order| {
                        hints.is_some_and(|hints| early_order(stage, order, hints).is_some())
                    })
                    .map(SwapOrderRecord::uid)
            })
            .collect()
    }

    pub(super) fn apply_order_hints(
        &mut self,
        results: Vec<HintResult>,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx) {
            return;
        }
        let now = Instant::now();
        let mut settled = Vec::new();
        for result in results {
            let tracking = self.tracking.entry(result.operation).or_default();
            tracking.keep_route(&result.route);
            if result.fee {
                tracking.fee_asked.insert(result.uid);
            }
            // An outcome ends the order's polling; otherwise the next poll waits longer.
            for (uid, outcome) in result.bridges {
                if outcome {
                    tracking.bridge_polls.remove(&uid);
                    settled.push(result.operation);
                } else {
                    tracking.back_off_bridge_poll(uid, now);
                }
            }
            if let Some((report, trade_block)) = result.report {
                // Only the reported order's own entry changes, so a late report on an order
                // that lost keeps the traded order's location.
                let known = tracking
                    .order_hints
                    .get(&result.uid)
                    .and_then(|hint| hint.trade_block);
                tracking.order_hints.insert(
                    result.uid,
                    SwapOrderHint {
                        report,
                        trade_block: if report.status == CowOrderStatusHint::Fulfilled {
                            trade_block.or(known)
                        } else {
                            None
                        },
                    },
                );
            }
            // What was learned early belongs to one reported trade block. Another block, or
            // an order no longer reported filled, drops it, and the new block is read again.
            let reported = tracking
                .order_hints
                .get(&result.uid)
                .filter(|hint| hint.report.status == CowOrderStatusHint::Fulfilled)
                .map(|hint| hint.trade_block);
            if tracking
                .early_hints
                .get(&result.uid)
                .is_some_and(|hint| Some(hint.trade_block) != reported)
            {
                tracking.early_hints.remove(&result.uid);
                tracking.bridge_polls.remove(&result.uid);
            }
            // An early answer for an older block is dropped. A provider's report of an end
            // stops the early polling of this block once the block was read, and for a
            // delivery whose fill block is read too, once that one was; anything else backs
            // off.
            if let Some(hint) = result.early
                && reported == Some(hint.trade_block)
            {
                tracking.early_hints.insert(result.uid, hint);
                if hint.ends_polling() && !hint.unread() {
                    tracking.bridge_polls.remove(&result.uid);
                } else {
                    tracking.back_off_bridge_poll(result.uid, now);
                }
            }
        }
        for operation in settled {
            self.reconcile_destination(operation, cx);
        }
        // Forget orders that are no longer tracked, such as those with a recorded outcome, and
        // what was learned early about an order once its settlement is recorded or its swap is
        // no longer shown open. The early requests' poll goes with it, so routine tracking of a
        // handed-off order starts without the early polling's delay or attempts.
        let early = self.early_orders();
        let records = &self.records;
        for (operation, tracking) in &mut self.tracking {
            let record = records
                .iter()
                .find(|record| record.operation() == *operation);
            tracking.early_hints.retain(|uid, _| {
                let keep = early.contains(uid);
                if !keep {
                    tracking.bridge_polls.remove(uid);
                }
                keep
            });
            tracking.bridge_polls.retain(|uid, _| {
                early.contains(uid)
                    || record.is_some_and(|record| {
                        record
                            .swap_bridges_to_track()
                            .any(|order| order.uid() == *uid)
                    })
            });
        }
        cx.notify();
    }

    pub(super) fn apply_observations(
        &mut self,
        results: Vec<ObservationResult>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Vec<ExecutorOperationId> {
        if !self.session_is_current(cx) {
            return Vec::new();
        }
        let confirmed = self.confirmed_block(cx);
        let mut catchup = Vec::new();
        for result in results {
            // The swap keeps a route its read opened, whatever the read found.
            self.tracking
                .entry(result.destination.unwrap_or(result.operation))
                .or_default()
                .keep_route(&result.route);
            // Another task updated the account during this background read. Keep the page
            // due for the next pass; no action failed and no stale result may advance it.
            if result
                .outcome
                .as_ref()
                .is_err_and(eyre::Report::is::<wallet_ops::ExecutorRecordChanged>)
            {
                continue;
            }
            // A destination account's result is kept with its swap.
            if let Some(origin) = result.destination {
                let tracking = self.tracking.entry(origin).or_default();
                match result.outcome {
                    Ok(setup) => {
                        tracking.error = None;
                        if let Some(setup) = setup {
                            tracking.destination_setup = Some(setup.status);
                        }
                    }
                    Err(error) => tracking.error = Some(format!("{error:#}")),
                }
                continue;
            }
            let tracking = self.tracking.entry(result.operation).or_default();
            match result.outcome {
                Ok(setup) => {
                    tracking.error = None;
                    // Only an order's history is paged. A page short of the confirmed block
                    // is followed at once.
                    if let Some(end) = result.range_end {
                        tracking.cursor = Some(end);
                        if confirmed.is_some_and(|block| end <= block) {
                            catchup.push(result.operation);
                        }
                    }
                    // Each setup observation replaces what the last one read at the latest
                    // block.
                    if let Some(setup) = setup {
                        tracking.setup = Some(setup.status);
                        tracking.inclusion = setup.inclusion;
                    }
                }
                Err(error) => tracking.error = Some(format!("{error:#}")),
            }
        }
        self.reload_records();
        // A setup the record no longer leaves unresolved needs no read at the latest block.
        let records = &self.records;
        for (operation, tracking) in &mut self.tracking {
            if records.iter().any(|record| {
                record.operation() == *operation
                    && wallet_ops::own_pending_swap_setup(record).is_none()
            }) {
                tracking.inclusion = None;
            }
        }
        self.reload_destinations(cx);
        // A cancellation is settled once canonical observation says which payload won.
        let settled = self
            .records
            .iter()
            .filter(|record| !self.stage(record).is_observed())
            .map(ExecutorRecord::operation)
            .collect::<Vec<_>>();
        for operation in settled {
            if let Some(tracking) = self.tracking.get_mut(&operation) {
                tracking.cancelling = false;
            }
        }
        // A swap that left its setup shows its order, and keeps no setup progress.
        let past_setup = self
            .records
            .iter()
            .filter(|record| {
                !matches!(
                    self.progress_stage(record),
                    SwapStage::SetupNotSent
                        | SwapStage::SetupSubmitting
                        | SwapStage::SetupPending
                        | SwapStage::SetupFailed
                        | SwapStage::SetupRetired
                )
            })
            .map(ExecutorRecord::operation)
            .collect::<Vec<_>>();
        for operation in past_setup {
            if let Some(tracking) = self.tracking.get_mut(&operation) {
                tracking.clear_setup_progress();
            }
        }
        self.continue_form_after_observation(window, cx);
        self.continue_approved_swaps(window, cx);
        cx.notify();
        catchup
    }

    /// Place the order of a swap approved with its setup in this session once every account it
    /// uses is ready, as [`Self::approved_use`] tells: its own account's setup is confirmed or
    /// included, and a private Bridge swap's destination account's is confirmed. After a
    /// restart, or once the user declined, its detail offers the step. Only this swap's detail
    /// may advance while a dialog is open.
    ///
    /// A swap whose approved spend doesn't fit yet, while its setup's change can still make it
    /// fit, sends nothing and keeps waiting, as the owner answers from its current notes. The
    /// next ready swap is placed meanwhile, and each pass asks again.
    pub(super) fn continue_approved_swaps(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() || self.form.is_some() || !self.session_is_current(cx) {
            return;
        }
        let candidates = self
            .records
            .iter()
            .filter_map(|record| {
                let operation = record.operation();
                let (_, approval) = self.approved_use(record)?;
                self.tracking
                    .get(&operation)
                    .is_some_and(|tracking| tracking.auto_place)
                    .then(|| {
                        let sell = approval
                            .tokens
                            .map(|tokens| tokens.sell)
                            .or_else(|| swap_tokens(record).map(|(sell, _)| sell));
                        (operation, sell, approval.bounds.spend_amount())
                    })
            })
            .collect::<Vec<_>>();
        let mut next = None;
        for (operation, sell, spend) in candidates {
            let waits = self.waits_for_setup_change(operation, sell, spend);
            self.tracking.entry(operation).or_default().waits_for_change = waits;
            if !waits && next.is_none() {
                next = Some(operation);
            }
        }
        if let Some(operation) = next
            && (!window.has_active_dialog(cx) || self.detail_is_active(operation, window, cx))
        {
            self.place_approved_order(operation, window, cx);
        }
    }

    /// Whether the automatic attempt to place the swap `operation`'s approved spend of `sell`
    /// waits for its setup's change, as the owner answers from one current snapshot of the
    /// wallet's notes. Without an answer the attempt runs, and reports what it finds.
    fn waits_for_setup_change(
        &self,
        operation: ExecutorOperationId,
        sell: Option<Address>,
        spend: U256,
    ) -> bool {
        let (Some(owner), Some(session), Some(sell)) =
            (self.private_owner(), self.private_session(), sell)
        else {
            return false;
        };
        matches!(
            owner.swap_placement_readiness(session, operation, sell, spend),
            Ok(SwapPlacementReadiness::WaitForSetupChange)
        )
    }
}
