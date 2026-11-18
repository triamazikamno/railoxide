use super::*;

impl PrivateSwapsView {
    pub(super) fn watch_origin_changes(
        session: Option<&Arc<WalletSession>>,
        owner: Option<&Arc<ExecutorOwner>>,
        cx: &Context<'_, Self>,
    ) -> Task<()> {
        let watches = owner.zip(session).map(|(owner, session)| {
            (
                owner.subscribe(),
                session.observation_rx.clone(),
                session.sync_tip_rx.clone(),
            )
        });
        cx.spawn(async move |this, cx| {
            let Some((mut changes, mut private_changes, mut tip_changes)) = watches else {
                return;
            };
            loop {
                let reload = tokio::select! {
                    changed = changes.changed() => changed.map(|()| true),
                    changed = private_changes.changed() => changed.map(|()| true),
                    changed = tip_changes.changed() => changed.map(|()| false),
                };
                let Ok(reload) = reload else {
                    break;
                };
                if this
                    .update(cx, |this, cx| {
                        if this.session_is_current(cx) {
                            if reload {
                                this.reload_records();
                                this.refresh_public_swap_records(cx);
                                this.reload_destinations(cx);
                                this.refresh_form_assets(cx);
                            }
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    pub(super) fn new_for_origin(
        root: WeakEntity<WalletRoot>,
        origin_chain_id: u64,
        session: Option<Arc<WalletSession>>,
        owner: Option<Arc<ExecutorOwner>>,
        runtime: tokio::runtime::Handle,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let changes_task = Self::watch_origin_changes(session.as_ref(), owner.as_ref(), cx);
        // Every change to the records, tracking or jobs notifies, so the open count follows
        // notifications. It reads the root, which is busy creating this view, so the first
        // count waits for the first notification; opening the dialog sends one.
        cx.observe_self(|this, cx| {
            let open = this.open_order_count(cx);
            this.open_orders = open;
            this.schedule_speed_up_reveal(cx);
        })
        .detach();
        let observation_wake = watch::channel(0).0;
        let mut wake = observation_wake.subscribe();
        let polling = cx.spawn_in(window, async move |this, cx| {
            let mut wait = MIN_SWAP_POLL_INTERVAL;
            loop {
                let mut woken = false;
                tokio::select! {
                    () = cx.background_executor().timer(wait) => {}
                    changed = wake.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        woken = true;
                    }
                }
                let Ok(work) = this.update_in(cx, |this, window, cx| {
                    // A recorded executed setup is ready without an account read, so the
                    // inclusion that woke this pass may leave no page to observe. The form
                    // continues here, as approved orders do on every pass.
                    if woken {
                        this.continue_form_after_observation(window, cx);
                    }
                    this.refresh_public_swap_records(cx);
                    this.track_public_swaps(window, cx);
                    this.continue_public_swap_after_setup(window, cx);
                    this.refresh_destinations(cx);
                    this.continue_approved_swaps(window, cx);
                    this.track_records();
                    let (work, pages) = match this.next_observations(cx) {
                        Some((runtime, owner, pages)) => (Some((runtime, owner)), pages),
                        None => (None, Vec::new()),
                    };
                    // Only the swaps due now are observed.
                    let pages = this.observation_schedule.poll_due(
                        pages,
                        ObservationPage::swap,
                        Instant::now(),
                    );
                    work.filter(|_| !pages.is_empty())
                        .map(|(runtime, owner)| (runtime, owner, pages))
                }) else {
                    break;
                };
                if let Some((runtime, owner, pages)) = work {
                    let polled = pages
                        .iter()
                        .map(ObservationPage::swap)
                        .collect::<BTreeSet<_>>();
                    let results = runtime.spawn(observe_pages(owner, pages)).await;
                    if this
                        .update_in(cx, |this, window, cx| {
                            let now = Instant::now();
                            this.observation_schedule.polled(&polled, now);
                            if let Ok(results) = results {
                                // A successful, lagging scan is drained at once. Caught-up
                                // and failed accounts keep their schedule.
                                for operation in this.apply_observations(results, window, cx) {
                                    this.observation_schedule.wake(operation, now);
                                }
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                let Ok(next) = this.update_in(cx, |this, _, _| {
                    this.observation_schedule
                        .next_due()
                        .into_iter()
                        .chain(this.public_schedule.next_due())
                        .min()
                }) else {
                    break;
                };
                wait = poll_wait(next);
            }
        });
        // Orderbook reports only add copy, so they poll on their own loop: a stalled orderbook
        // never delays canonical observation or order placement. At most one batch is in flight.
        let hint_polling = cx.spawn_in(window, async move |this, cx| {
            // Each swap's orders are asked about on its own schedule.
            let mut schedule = SwapSchedule::<ExecutorOperationId>::new();
            let mut first_wait = Some(MIN_SWAP_POLL_INTERVAL);
            loop {
                cx.background_executor()
                    .timer(
                        first_wait
                            .take()
                            .unwrap_or_else(|| poll_wait(schedule.next_due())),
                    )
                    .await;
                let Ok(hints) = this.update_in(cx, |this, _, cx| {
                    this.track_records();
                    this.next_order_hints(cx)
                }) else {
                    break;
                };
                let (work, requests) = match hints {
                    Some((runtime, owner, requests)) => (Some((runtime, owner)), requests),
                    None => (None, Vec::new()),
                };
                let requests =
                    schedule.poll_due(requests, |request| request.operation, Instant::now());
                let Some((runtime, owner)) = work.filter(|_| !requests.is_empty()) else {
                    continue;
                };
                let _ = this.update_in(cx, |this, _, cx| {
                    if let Some(root) = this.root.upgrade() {
                        root.update(cx, |root, _| {
                            root.activate_chains_background(requests.iter().flat_map(|request| {
                                // A non-EVM delivery has no chain to activate.
                                request
                                    .bridges
                                    .iter()
                                    .filter_map(|(_, chain)| {
                                        chain.as_ref().map(|chain| chain.chain_id)
                                    })
                                    .chain(request.destination.iter().map(|chain| chain.chain_id))
                            }));
                        });
                    }
                });
                let polled = requests
                    .iter()
                    .map(|request| request.operation)
                    .collect::<BTreeSet<_>>();
                let results = runtime.spawn(fetch_order_hints(owner, requests)).await;
                schedule.polled(&polled, Instant::now());
                if this
                    .update_in(cx, |this, _, cx| {
                        if let Ok(results) = results {
                            this.apply_order_hints(results, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut view = Self {
            root,
            origin_chain_id,
            session,
            owner,
            public_records: Vec::new(),
            public_tracking: BTreeSet::new(),
            public_schedule: SwapSchedule::new(),
            public_destination_balances: BTreeMap::new(),
            public_railgun: BTreeMap::new(),
            public_synced: BTreeSet::new(),
            public_tracking_failures: BTreeMap::new(),
            public_permit_used_up: BTreeSet::new(),
            public_authorization: None,
            public_execution: None,
            public_job: None,
            public_running: None,
            public_status: None,
            runtime,
            active: true,
            records: Vec::new(),
            setup_inclusions: None,
            observation_wake,
            observation_schedule: SwapSchedule::new(),
            tracking: BTreeMap::new(),
            destinations: BTreeMap::new(),
            attributions: BTreeMap::new(),
            speed_up_refusals: BTreeMap::new(),
            speed_up_reveal: None,
            open_orders: 0,
            dialog: None,
            orders_filter: None,
            orders_list: None,
            form: None,
            setup_retry: None,
            cancel: None,
            pending_authorization: None,
            reapproval: None,
            cancelled: None,
            job: None,
            job_revision: 0,
            error: None,
            origin_changes: changes_task,
            _polling: polling,
            _hint_polling: hint_polling,
        };
        view.reload_records();
        view
    }

    /// The origin network's private session, which every private swap spends from. `None` in
    /// a view that only tracks swaps paid from Public accounts, which need none there.
    pub(super) const fn private_session(&self) -> Option<&Arc<WalletSession>> {
        self.session.as_ref()
    }

    /// The origin network's executor owner, as [`Self::private_session`] is available.
    pub(super) const fn private_owner(&self) -> Option<&Arc<ExecutorOwner>> {
        self.owner.as_ref()
    }

    /// The wallet's network context, which opens the routes of swaps and checks.
    pub(super) fn http(&self, cx: &gpui::App) -> Option<wallet_ops::HttpContext> {
        Some(self.root.upgrade()?.read(cx).http.clone())
    }

    /// Gives every loaded swap a tracking entry, so that both polling loops share the route
    /// either of them opens for it.
    pub(super) fn track_records(&mut self) {
        for record in &self.records {
            self.tracking.entry(record.operation()).or_default();
        }
    }

    /// The route the swap `operation` is read over: the one it keeps, or the one its tracking
    /// entry opens once for both polling loops.
    pub(super) fn swap_route(
        &self,
        http: &wallet_ops::HttpContext,
        operation: ExecutorOperationId,
    ) -> SwapRoute {
        SwapRoute {
            http: http.clone(),
            orderbook: self
                .tracking
                .get(&operation)
                .map_or_else(Arc::default, SwapTracking::route_cell),
        }
    }

    pub(super) fn public_record_chain(&self, record: &ExecutorRecord) -> Option<u64> {
        self.public_records.iter().find_map(|(chain, candidate)| {
            (candidate.operation() == record.operation()).then_some(*chain)
        })
    }

    pub(super) fn session_is_current(&self, cx: &gpui::App) -> bool {
        self.active
            && self.root.upgrade().is_some_and(|root| {
                let root = root.read(cx);
                root.last_evm_chain == self.origin_chain_id
                    && root.view_session.is_some()
                    && self
                        .session
                        .as_ref()
                        .is_none_or(|session| root.stealth_session_is_current(session))
            })
    }

    pub(super) fn reload_records(&mut self) {
        // Debug UI fixture: its synthesized records aren't read over.
        if ui_fixture::holds_swaps() {
            return;
        }
        let Some(owner) = self.owner.as_ref() else {
            return;
        };
        match owner.records() {
            Ok(records) => {
                let selected = self.form.as_ref().and_then(SwapForm::operation);
                let mut records = records
                    .into_iter()
                    .filter(|record| {
                        is_swap_record(record)
                            || model::cancelled_swap_uses(record).next().is_some()
                            || Some(record.operation()) == selected
                            || self.pending_order(record).is_some()
                    })
                    .collect::<Vec<_>>();
                records
                    .sort_by_key(|record| std::cmp::Reverse((record.created_at(), record.index())));
                // Only a setup that private sync newly shows in a transaction recorded for it
                // wakes the observation loop: it makes that swap's account check due at once,
                // with its read at the latest block, also beside an order placed on it.
                // Waking on every owner change would spin, since each pass records its own
                // observations.
                let inclusions = {
                    let observation = self
                        .session
                        .as_ref()
                        .map(|session| session.observation_rx.borrow());
                    let utxos = observation
                        .as_ref()
                        .map_or(&[][..], |observation| &observation.snapshot.utxos[..]);
                    synced_setup_inclusions(&records, utxos)
                };
                if let Some(known) = &self.setup_inclusions
                    && !inclusions.is_subset(known)
                {
                    let now = Instant::now();
                    for &operation in inclusions.difference(known) {
                        self.observation_schedule.wake(operation, now);
                    }
                    self.observation_wake
                        .send_modify(|wakes| *wakes = wakes.wrapping_add(1));
                }
                self.setup_inclusions = Some(inclusions);
                self.attributions.extend(
                    records.iter().filter_map(|record| {
                        Some((record.operation(), owner.attribution(record)?))
                    }),
                );
                // The owner's rule is the only one. It reads local records and, for an order
                // on a setup the record leaves unresolved, this session's last read showing
                // that setup included. An expired order can't pass it, so only the others are
                // asked about.
                let now = now_unix();
                self.speed_up_refusals = records
                    .iter()
                    .filter_map(|record| {
                        let order = record.swap()?.orders().last()?;
                        if u64::from(order.valid_to()) <= now {
                            return None;
                        }
                        let inclusion = self
                            .tracking
                            .get(&record.operation())
                            .and_then(|tracking| tracking.inclusion);
                        let refusal = owner
                            .swap_speed_up_refusal(record.operation(), order.uid(), inclusion)
                            .ok()?;
                        Some((record.operation(), (order.uid(), refusal)))
                    })
                    .collect();
                self.records = records;
            }
            // Keep the last records; the owner reports the same problem to every caller.
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    pub(super) fn record(&self, operation: ExecutorOperationId) -> Option<&ExecutorRecord> {
        self.records
            .iter()
            .find(|record| record.operation() == operation)
    }
}
