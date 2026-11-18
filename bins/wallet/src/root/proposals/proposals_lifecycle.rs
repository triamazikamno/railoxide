use super::*;

impl WalletRoot {
    pub(in crate::root) fn open_proposals(&mut self, cx: &mut Context<'_, Self>) {
        self.active_activity = crate::root::sidebar::Activity::Proposals;
        self.clean_governance_participants();
        self.proposals.focus_list_on_render = true;
        if self.proposals.chain_time_anchor.is_some() {
            self.start_proposals_time_tick(cx);
        }
        if self.governance.tab == crate::root::governance::GovernanceTab::Proposals
            && !self.proposals.checked
            && !self.proposals.loading
        {
            self.start_proposals_refresh(false, cx);
        } else if self.governance.tab == crate::root::governance::GovernanceTab::Staking
            && matches!(
                self.governance.staking.status,
                crate::root::governance::StakingRefreshStatus::Idle
            )
        {
            self.start_staking_refresh(cx);
        } else if self.governance.tab == crate::root::governance::GovernanceTab::Proposals {
            let selected = self.proposals.selected.as_ref().and_then(|selection| {
                self.proposals.rows(selection.page).and_then(|rows| {
                    rows.iter()
                        .find(|row| row.identity() == selection.identity)
                        .map(|row| row.proposal.clone())
                })
            });
            if let Some(proposal) = selected.as_ref() {
                self.show_proposal_participation(proposal);
            }
        }
        cx.notify();
    }

    fn start_proposals_time_tick(&mut self, cx: &mut Context<'_, Self>) {
        if self.proposals.time_tick_active {
            return;
        }
        self.proposals.time_tick_active = true;
        let generation = self.proposals.generation;
        let (tick_tx, tick_rx) = oneshot::channel();
        let join = self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_mins(1)).await;
            let _ = tick_tx.send(());
        });
        self.proposals.task_tracker.track(join);
        cx.spawn(async move |this, cx| {
            let Ok(()) = tick_rx.await else {
                return;
            };
            let _ = this.update(cx, |root, cx| {
                root.proposals.time_tick_active = false;
                if root.proposals.generation == generation
                    && root.active_activity == crate::root::sidebar::Activity::Proposals
                {
                    cx.notify();
                    root.start_proposals_time_tick(cx);
                }
            });
        })
        .detach();
        cx.notify();
    }
    pub(in crate::root) fn invalidate_proposals_chain(&mut self, chain_id: u64) {
        self.proposals.invalidate(chain_id);
        if self.active_activity == crate::root::sidebar::Activity::Proposals {
            self.proposals.focus_list_on_render = true;
        }
    }
    pub(in crate::root) fn start_proposals_refresh(
        &mut self,
        manual: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if manual {
            self.proposals.prepare_manual_refresh();
        }
        self.proposals.task_tracker.abort_and_clear();
        self.proposals.time_tick_active = false;
        self.proposals
            .cancel_active_page(self.proposals.current_page);
        self.proposals.cancel_prefetch();
        self.proposals.hydrating_pages.clear();
        self.proposals.loading = false;
        let Some(chain_id) = self.selected_evm_chain() else {
            // Tron has no governance: show the not-deployed state without reading.
            self.proposals.invalidate(self.last_evm_chain);
            self.proposals.checked = true;
            cx.notify();
            return;
        };
        let requested_page = if manual {
            self.proposals.current_page
        } else {
            0
        };
        self.proposals.chain_id = chain_id;
        self.proposals.loading = true;
        self.proposals.refreshing = manual;
        self.proposals.error = None;
        self.proposals.loading_pages.clear();
        self.proposals.request_generation = self.proposals.request_generation.wrapping_add(1);
        let request_generation = self.proposals.request_generation;
        let chain_generation = self.proposals.generation;
        let http = self.http.clone();
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let (result_tx, result_rx) = oneshot::channel();
        let join = self.runtime.spawn(async move {
            let result =
                match governance_route(chain_id, &effective_chain, RpcTrafficClass::General) {
                    Ok(route) => load_initial(chain_id, requested_page, &route, &http).await,
                    Err(error) => Err(eyre::eyre!(error)),
                };
            let _ = result_tx.send(result);
        });
        self.proposals.task_tracker.track(join);
        cx.spawn(async move |this, cx| {
            let Ok(result) = result_rx.await else {
                return;
            };
            let _ = this.update(cx, |root, cx| {
                if root.selected_evm_chain() != Some(chain_id)
                    || root.proposals.generation != chain_generation
                    || root.proposals.request_generation != request_generation
                {
                    return;
                }
                root.proposals.loading = false;
                root.proposals.refreshing = false;
                match result {
                    Ok(load) => {
                        root.proposals.checked = true;
                        root.proposals.error = None;
                        root.proposals.overview = load.overview;
                        root.proposals.chain_time_anchor = load.chain_time_anchor;
                        root.proposals.total_pages = load.total_pages;
                        root.proposals
                            .replace_refreshed_page(requested_page, load.page, load.rows);
                        let selected_proposal =
                            root.proposals.selected.as_ref().and_then(|selection| {
                                root.proposals.rows(selection.page).and_then(|rows| {
                                    rows.iter()
                                        .find(|row| row.identity() == selection.identity)
                                        .map(|row| row.proposal.clone())
                                })
                            });
                        if let Some(proposal) = selected_proposal.as_ref() {
                            root.show_proposal_participation(proposal);
                        }
                        if root.proposals.overview.is_some() {
                            root.start_proposals_time_tick(cx);
                            root.resume_active_proposals_page(load.page, cx);
                        }
                    }
                    Err(error) => {
                        root.proposals.error = Some(Arc::from(format_report_chain(&error)));
                        if root.proposals.chain_time_anchor.is_some()
                            && root.active_activity == crate::root::sidebar::Activity::Proposals
                        {
                            root.start_proposals_time_tick(cx);
                        }
                        let page = root.proposals.current_page;
                        root.resume_active_proposals_page(page, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(in crate::root) fn select_proposals_page(
        &mut self,
        page: usize,
        cx: &mut Context<'_, Self>,
    ) {
        if page >= self.proposals.total_pages {
            return;
        }
        if page != self.proposals.current_page {
            self.proposals
                .cancel_active_page(self.proposals.current_page);
            self.proposals.cancel_prefetch();
        }
        self.proposals.current_page = page;
        self.proposals.selected = None;
        self.close_proposal_action(cx);
        self.proposals.expanded_calldata.clear();
        self.proposals.focus_list_on_render = true;
        if self.proposals.rows(page).is_none() {
            self.request_proposals_page(page, false, cx);
        } else {
            self.resume_active_proposals_page(page, cx);
        }
        cx.notify();
    }
    pub(in crate::root) fn select_proposal(
        &mut self,
        page: usize,
        identity: &ProposalIdentity,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let table_count =
            self.proposals
                .rows(page)
                .and_then(|rows| rows.iter().find(|row| row.identity().eq(identity)))
                .and_then(ResolvedProposal::presentation)
                .and_then(|presentation| match presentation {
                    ProposalPresentation::Prepared(prepared) => Some(prepared.table_count),
                    ProposalPresentation::RawParseFallback(_)
                    | ProposalPresentation::TooComplex => None,
                });
        if let Some(table_count) = table_count {
            self.proposals
                .ensure_table_scroll_handles(identity, table_count);
        }
        self.proposals.expanded_calldata.clear();
        self.proposals.selected = Some(ProposalSelection {
            page,
            identity: identity.clone(),
            tab: ProposalDetailTab::default(),
            participation_expanded: false,
        });
        self.proposals.detail_scroll_handle = ScrollHandle::new();
        let selected_proposal = self.proposals.rows(page).and_then(|rows| {
            rows.iter()
                .find(|row| row.identity().eq(identity))
                .map(|row| row.proposal.clone())
        });
        if let Some(proposal) = selected_proposal.as_ref() {
            self.show_proposal_participation(proposal);
        }
        self.proposal_detail_focus.focus(window, cx);
        cx.notify();
    }
    pub(in crate::root) fn clear_selected_proposal(&mut self, cx: &mut Context<'_, Self>) {
        self.proposals.selected = None;
        self.close_proposal_action(cx);
        self.proposals.expanded_calldata.clear();
        self.proposals.focus_list_on_render = true;
        cx.notify();
    }
    pub(super) fn select_proposal_detail_tab(
        &mut self,
        tab: ProposalDetailTab,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(selection) = self.proposals.selected.as_mut() {
            selection.tab = tab;
            cx.notify();
        }
    }
    pub(super) fn toggle_proposal_participation(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(selection) = self.proposals.selected.as_mut() {
            selection.participation_expanded = !selection.participation_expanded;
            cx.notify();
        }
    }
    pub(super) fn toggle_proposal_calldata(
        &mut self,
        identity: ProposalIdentity,
        ordinal: usize,
        cx: &mut Context<'_, Self>,
    ) {
        let key = ProposalActionIdentity {
            proposal: identity,
            ordinal,
        };
        if !self.proposals.expanded_calldata.remove(&key) {
            self.proposals.expanded_calldata.insert(key);
        }
        cx.notify();
    }
    pub(in crate::root) fn retry_proposals(&mut self, cx: &mut Context<'_, Self>) {
        self.proposals.error = None;
        let page = self.proposals.current_page;
        if self.proposals.overview.is_some()
            && page < self.proposals.total_pages
            && self.proposals.rows(page).is_none()
        {
            self.request_proposals_page(page, false, cx);
        } else {
            self.start_proposals_refresh(true, cx);
        }
        cx.notify();
    }
    fn schedule_proposals_prefetch(&mut self, current: usize, cx: &Context<'_, Self>) {
        let Some(page) = self.proposals.prefetch_candidate(current) else {
            return;
        };
        if self.proposals.prefetch_page == Some(page) {
            return;
        }
        self.proposals.cancel_prefetch();
        self.proposals.prefetch_page = Some(page);
        if self.proposals.rows(page).is_some() {
            self.start_proposals_hydration(page, true, cx);
        } else {
            self.request_proposals_page(page, true, cx);
        }
    }
    fn request_proposals_page(&mut self, page: usize, prefetch: bool, cx: &Context<'_, Self>) {
        let Some(overview) = self.proposals.overview.clone() else {
            return;
        };
        let Some(chain_id) = self.selected_evm_chain() else {
            return;
        };
        if self.proposals.pages.contains_key(&page) || !self.proposals.loading_pages.insert(page) {
            return;
        }
        let chain_generation = self.proposals.generation;
        let request_generation = self.proposals.request_generation;
        let work_token = if prefetch {
            self.proposals.prefetch_token
        } else {
            self.proposals.active_page_token
        };
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let http = self.http.clone();
        tracing::debug!(
            target: LOG_TARGET,
            chain_id,
            page,
            prefetch,
            "loading governance proposal page"
        );
        let (result_tx, result_rx) = oneshot::channel();
        let join = self.runtime.spawn(async move {
            let result =
                match governance_route(chain_id, &effective_chain, RpcTrafficClass::General) {
                    Ok(route) => load_page(&overview, page, &route, &http).await,
                    Err(error) => Err(eyre::eyre!(error)),
                };
            let _ = result_tx.send(result);
        });
        if prefetch {
            self.proposals.prefetch_task_tracker.track(join);
        } else {
            self.proposals.active_page_task_tracker.track(join);
        }
        cx.spawn(async move |this, cx| {
            let Ok(result) = result_rx.await else {
                return;
            };
            let _ = this.update(cx, |root, cx| {
                if root.selected_evm_chain() != Some(chain_id)
                    || root.proposals.generation != chain_generation
                    || root.proposals.request_generation != request_generation
                {
                    return;
                }
                if !root.proposals.owns_page_work(page, prefetch, work_token) {
                    return;
                }
                root.proposals.loading_pages.remove(&page);
                match result {
                    Ok(rows) => {
                        root.proposals.pages.insert(
                            page,
                            ProposalsPage {
                                rows: Arc::new(rows),
                            },
                        );
                        if prefetch {
                            root.start_proposals_hydration(page, true, cx);
                        } else {
                            root.resume_active_proposals_page(page, cx);
                        }
                        if !prefetch && page == root.proposals.current_page {
                            root.proposals.error = None;
                            let selected_proposal =
                                root.proposals.selected.as_ref().and_then(|selection| {
                                    root.proposals.rows(selection.page).and_then(|rows| {
                                        rows.iter()
                                            .find(|row| row.identity() == selection.identity)
                                            .map(|row| row.proposal.clone())
                                    })
                                });
                            if let Some(proposal) = selected_proposal.as_ref() {
                                root.show_proposal_participation(proposal);
                            }
                        }
                    }
                    Err(error) => {
                        if !prefetch {
                            root.proposals.error = Some(Arc::from(format_report_chain(&error)));
                        } else if root.proposals.owns_page_work(page, true, work_token) {
                            root.proposals.prefetch_page = None;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::root) fn refresh_selected_proposal_page(&mut self, cx: &mut Context<'_, Self>) {
        let page = self
            .proposals
            .selected
            .as_ref()
            .map_or(self.proposals.current_page, |selection| selection.page);
        // A load started before the refresh must not land.
        self.proposals.cancel_active_page(page);
        self.proposals.pages.remove(&page);
        self.proposals.proposal_lists.remove(&page);
        self.request_proposals_page(page, false, cx);
        cx.notify();
    }

    /// Drops the cached pages showing the proposal of `key` after an action on it, and reloads
    /// the current page when it was dropped or its load was cancelled.
    pub(in crate::root) fn evict_acted_proposal(
        &mut self,
        key: &crate::root::governance_action::ProposalParticipationKey,
        cx: &Context<'_, Self>,
    ) {
        let identity = ProposalIdentity {
            contract_version: key.version,
            contract_address: key.contract,
            index: key.index,
        };
        if self
            .proposals
            .evict_proposal(key.context.chain_id, &identity)
        {
            self.request_proposals_page(self.proposals.current_page, false, cx);
        }
    }
    fn resume_active_proposals_page(&mut self, page: usize, cx: &Context<'_, Self>) {
        self.start_proposals_hydration(page, false, cx);
        self.schedule_proposals_prefetch(page, cx);
    }
    fn start_proposals_hydration(&mut self, page: usize, prefetch: bool, cx: &Context<'_, Self>) {
        let Some(rows) = self.proposals.pending_document_rows(page) else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        let Some(chain_id) = self.selected_evm_chain() else {
            return;
        };
        if !self.proposals.hydrating_pages.insert(page) {
            return;
        }
        let chain_generation = self.proposals.generation;
        let request_generation = self.proposals.request_generation;
        let work_token = if prefetch {
            self.proposals.prefetch_token
        } else {
            self.proposals.active_page_token
        };
        let http = self.http.clone();
        let vault_store = self.vault_store.clone();
        let semaphore = Arc::clone(&self.proposals.document_semaphore);
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let join = self.runtime.spawn(async move {
            hydrate_page(rows, vault_store.as_ref(), &http, semaphore, completion_tx).await;
        });
        if prefetch {
            self.proposals.prefetch_task_tracker.track(join);
        } else {
            self.proposals.active_page_task_tracker.track(join);
        }
        cx.spawn(async move |this, cx| {
            while let Some(first) = completion_rx.recv().await {
                let mut completions = vec![first];
                while let Ok(completion) = completion_rx.try_recv() {
                    completions.push(completion);
                }
                let _ = this.update(cx, |root, cx| {
                    if root.selected_evm_chain() != Some(chain_id)
                        || root.proposals.generation != chain_generation
                        || root.proposals.request_generation != request_generation
                    {
                        return;
                    }
                    if !root.proposals.owns_page_work(page, prefetch, work_token) {
                        return;
                    }
                    let mut visible = false;
                    for completion in completions {
                        visible |= root.proposals.apply_document(page, completion);
                    }
                    if visible {
                        cx.notify();
                    }
                });
            }
            let _ = this.update(cx, |root, cx| {
                if root.selected_evm_chain() != Some(chain_id)
                    || root.proposals.generation != chain_generation
                    || root.proposals.request_generation != request_generation
                {
                    return;
                }
                if !root.proposals.owns_page_work(page, prefetch, work_token) {
                    return;
                }
                root.proposals.hydrating_pages.remove(&page);
                if !prefetch && page == root.proposals.current_page {
                    root.schedule_proposals_prefetch(page, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn ensure_proposal_list(
        &mut self,
        page: usize,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<ListState<ProposalListDelegate>> {
        if let Some(list) = self.proposals.proposal_lists.get(&page) {
            return list.clone();
        }
        let list =
            cx.new(|cx| ListState::new(ProposalListDelegate::new(root.downgrade()), window, cx));
        let observed_root = root.clone();
        list.update(cx, |_list, cx| {
            cx.observe(&observed_root, |_list, _root, cx| cx.notify())
                .detach();
        });
        self.proposals.proposal_lists.insert(page, list.clone());
        list
    }
}
