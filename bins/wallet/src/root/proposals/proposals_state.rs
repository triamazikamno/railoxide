use super::*;

impl ProposalDocumentState {
    const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

impl ResolvedProposal {
    pub(in crate::root) const fn identity(&self) -> ProposalIdentity {
        ProposalIdentity {
            contract_version: self.proposal.contract_version,
            contract_address: self.proposal.contract_address,
            index: self.proposal.index,
        }
    }

    pub(super) const fn document(&self) -> Option<&GovernanceDocument> {
        match &self.document {
            ProposalDocumentState::Pending => None,
            ProposalDocumentState::Resolved { document, .. } => Some(document),
        }
    }

    pub(super) const fn presentation(&self) -> Option<&ProposalPresentation> {
        match &self.document {
            ProposalDocumentState::Pending => None,
            ProposalDocumentState::Resolved { presentation, .. } => Some(presentation),
        }
    }

    pub(super) const fn has_pending_document(&self) -> bool {
        self.document.is_pending()
    }

    pub(super) fn status(&self, chain_time: U256) -> GovernanceProposalStatus {
        derive_governance_proposal_status(&self.proposal, &self.rules, chain_time)
            .expect("validated governance rules must derive a status")
    }
}

impl ChainTimeAnchor {
    fn current(&self) -> U256 {
        self.current_at(self.captured_at.elapsed().as_secs())
    }

    pub(super) fn current_at(&self, elapsed_secs: u64) -> U256 {
        advance_chain_time(self.chain_time, elapsed_secs)
    }
}

fn advance_chain_time(chain_time: U256, elapsed_secs: u64) -> U256 {
    chain_time
        .checked_add(U256::from(elapsed_secs))
        .unwrap_or(U256::MAX)
}

impl ProposalListDelegate {
    pub(super) const fn new(root: WeakEntity<WalletRoot>) -> Self {
        Self {
            root,
            selected: None,
        }
    }
}

impl ListDelegate for ProposalListDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, cx: &gpui::App) -> usize {
        self.root.upgrade().map_or(0, |root| {
            root.read_with(cx, |root, _| {
                root.proposals
                    .rows(root.proposals.current_page)
                    .map_or(0, <[_]>::len)
            })
        })
    }

    fn render_item(
        &mut self,
        ix: gpui_component::IndexPath,
        _window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) -> Option<Self::Item> {
        self.root.upgrade().and_then(|root| {
            root.read_with(cx, |root, _| {
                let page = root.proposals.current_page;
                let chain_time = root.proposals.chain_time();
                root.proposals
                    .rows(page)
                    .and_then(|rows| rows.get(ix.row))
                    .map(|proposal| {
                        WalletRoot::render_proposal_row(
                            proposal,
                            chain_time,
                            self.selected == Some(ix),
                        )
                    })
            })
        })
    }

    fn set_selected_index(
        &mut self,
        ix: Option<gpui_component::IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) {
        self.selected = ix;
    }

    fn confirm(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) {
        let Some(ix) = self.selected else {
            return;
        };
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let Some((page, identity)) = root.read_with(cx, |root, _| {
            let page = root.proposals.current_page;
            let identity = root
                .proposals
                .rows(page)
                .and_then(|rows| rows.get(ix.row))
                .map(ResolvedProposal::identity);
            identity.map(|identity| (page, identity))
        }) else {
            return;
        };
        root.update(cx, |root, cx| {
            root.select_proposal(page, &identity, window, cx);
        });
    }
}

impl ProposalTaskTracker {
    pub(super) fn track(&mut self, task: tokio::task::JoinHandle<()>) {
        self.tasks.retain(|task| !task.is_finished());
        self.tasks.push(task);
    }

    fn abort_all(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }

    pub(super) fn abort_and_clear(&mut self) {
        self.abort_all();
        self.tasks.clear();
    }

    fn take(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        std::mem::take(&mut self.tasks)
    }
}

impl Drop for ProposalTaskTracker {
    fn drop(&mut self) {
        self.abort_all();
    }
}

impl ProposalCleanup {
    #[cfg(test)]
    pub(in crate::root) const fn empty() -> Self {
        Self { tasks: Vec::new() }
    }

    pub(in crate::root) async fn shutdown(self) -> Result<(), String> {
        for task in &self.tasks {
            task.abort();
        }
        let mut failures = 0;
        for task in self.tasks {
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                failures += 1;
                tracing::warn!(target: LOG_TARGET, %error, "proposal worker failed during cleanup");
            }
        }
        if failures == 0 {
            Ok(())
        } else {
            Err(format!(
                "{failures} proposal worker failures during cleanup"
            ))
        }
    }
}

impl ProposalsState {
    pub(in crate::root) fn new(chain_id: u64) -> Self {
        Self {
            chain_id,
            generation: 0,
            request_generation: 0,
            checked: false,
            overview: None,
            chain_time_anchor: None,
            total_pages: 0,
            current_page: 0,
            pages: BTreeMap::new(),
            loading_pages: BTreeSet::new(),
            hydrating_pages: BTreeSet::new(),
            prefetch_page: None,
            active_page_token: 0,
            prefetch_token: 0,
            proposal_lists: BTreeMap::new(),
            focus_list_on_render: false,
            table_scroll_handles: BTreeMap::new(),
            expanded_calldata: BTreeSet::new(),
            detail_scroll_handle: ScrollHandle::new(),
            loading: false,
            refreshing: false,
            error: None,
            selected: None,
            time_tick_active: false,
            task_tracker: ProposalTaskTracker { tasks: Vec::new() },
            active_page_task_tracker: ProposalTaskTracker { tasks: Vec::new() },
            prefetch_task_tracker: ProposalTaskTracker { tasks: Vec::new() },
            document_semaphore: Arc::new(Semaphore::new(DOCUMENT_RESOLUTION_CONCURRENCY)),
        }
    }
    pub(super) fn invalidate(&mut self, chain_id: u64) {
        self.task_tracker.abort_and_clear();
        self.cancel_active_page(self.current_page);
        self.cancel_prefetch();
        self.chain_id = chain_id;
        self.generation = self.generation.wrapping_add(1);
        self.request_generation = self.request_generation.wrapping_add(1);
        self.checked = false;
        self.overview = None;
        self.chain_time_anchor = None;
        self.total_pages = 0;
        self.current_page = 0;
        self.pages.clear();
        self.loading_pages.clear();
        self.hydrating_pages.clear();
        self.proposal_lists.clear();
        self.focus_list_on_render = false;
        self.table_scroll_handles.clear();
        self.expanded_calldata.clear();
        self.detail_scroll_handle = ScrollHandle::new();
        self.loading = false;
        self.refreshing = false;
        self.error = None;
        self.selected = None;
        self.time_tick_active = false;
    }
    pub(in crate::root) fn take_cleanup(&mut self) -> ProposalCleanup {
        self.generation = self.generation.wrapping_add(1);
        self.request_generation = self.request_generation.wrapping_add(1);
        self.active_page_token = self.active_page_token.wrapping_add(1);
        self.prefetch_token = self.prefetch_token.wrapping_add(1);
        self.loading_pages.clear();
        self.hydrating_pages.clear();
        self.prefetch_page = None;
        self.table_scroll_handles.clear();
        self.expanded_calldata.clear();
        self.detail_scroll_handle = ScrollHandle::new();
        self.loading = false;
        self.refreshing = false;
        self.time_tick_active = false;
        let mut tasks = self.task_tracker.take();
        tasks.extend(self.active_page_task_tracker.take());
        tasks.extend(self.prefetch_task_tracker.take());
        ProposalCleanup { tasks }
    }
    pub(super) fn cancel_prefetch(&mut self) {
        self.prefetch_task_tracker.abort_and_clear();
        self.prefetch_token = self.prefetch_token.wrapping_add(1);
        if let Some(page) = self.prefetch_page.take() {
            self.loading_pages.remove(&page);
            self.hydrating_pages.remove(&page);
        }
    }
    pub(super) fn cancel_active_page(&mut self, page: usize) {
        self.active_page_task_tracker.abort_and_clear();
        self.active_page_token = self.active_page_token.wrapping_add(1);
        self.loading_pages.remove(&page);
        self.hydrating_pages.remove(&page);
    }
    /// Drops every cached page of `chain_id` showing `identity`, the prefetch, and the current
    /// page's work when that page was dropped or is still loading, since that work may carry the
    /// proposal's stale data. Returns whether the current page must be requested again.
    pub(super) fn evict_proposal(&mut self, chain_id: u64, identity: &ProposalIdentity) -> bool {
        if chain_id != self.chain_id {
            return false;
        }
        let stale = self
            .pages
            .iter()
            .filter(|(_, page)| page.rows.iter().any(|row| row.identity() == *identity))
            .map(|(page, _)| *page)
            .collect::<Vec<_>>();
        for page in &stale {
            self.pages.remove(page);
            self.proposal_lists.remove(page);
            self.loading_pages.remove(page);
            self.hydrating_pages.remove(page);
        }
        self.cancel_prefetch();
        let current = self.current_page;
        let reload = stale.contains(&current) || self.loading_pages.contains(&current);
        if reload {
            self.cancel_active_page(current);
        }
        reload
    }
    pub(super) fn owns_page_work(&self, page: usize, prefetch: bool, token: u64) -> bool {
        if prefetch {
            self.prefetch_page == Some(page) && self.prefetch_token == token
        } else {
            self.current_page == page && self.active_page_token == token
        }
    }
    pub(in crate::root) fn rows(&self, page: usize) -> Option<&[ResolvedProposal]> {
        self.pages.get(&page).map(|p| p.rows.as_slice())
    }

    pub(in crate::root) fn selected_proposal(&self) -> Option<&GovernanceProposal> {
        let selection = self.selected.as_ref()?;
        self.rows(selection.page)
            .and_then(|rows| rows.iter().find(|row| row.identity() == selection.identity))
            .map(|row| &row.proposal)
    }

    pub(super) fn chain_time(&self) -> U256 {
        self.chain_time_anchor
            .as_ref()
            .map_or(U256::ZERO, ChainTimeAnchor::current)
    }

    pub(super) fn pending_document_rows(&self, page: usize) -> Option<Vec<ResolvedProposal>> {
        self.pages.get(&page).map(|page| {
            page.rows
                .iter()
                .filter(|row| row.has_pending_document())
                .cloned()
                .collect()
        })
    }

    pub(super) fn page_is_terminal(&self, page: usize) -> bool {
        self.pages
            .get(&page)
            .is_some_and(|page| page.rows.iter().all(|row| !row.has_pending_document()))
    }

    pub(super) fn apply_document(&mut self, page: usize, completion: DocumentCompletion) -> bool {
        let Some(page_rows) = self.pages.get_mut(&page) else {
            return false;
        };
        let Some(row) = Arc::make_mut(&mut page_rows.rows)
            .iter_mut()
            .find(|row| row.identity() == completion.identity)
        else {
            return false;
        };
        let table_count = match &completion.presentation {
            ProposalPresentation::Prepared(prepared) => prepared.table_count,
            ProposalPresentation::RawParseFallback(_) | ProposalPresentation::TooComplex => 0,
        };
        row.document = ProposalDocumentState::Resolved {
            document: completion.document,
            presentation: completion.presentation,
        };
        self.ensure_table_scroll_handles(&completion.identity, table_count);
        self.selected.as_ref().is_some_and(|selected| {
            selected.page == page && selected.identity == completion.identity
        }) || self.current_page == page
    }

    pub(super) fn ensure_table_scroll_handles(
        &mut self,
        identity: &ProposalIdentity,
        table_count: usize,
    ) {
        for ordinal in 0..table_count {
            self.table_scroll_handles
                .entry(proposal_table_scroll_key(identity, ordinal))
                .or_default();
        }
    }

    pub(super) fn prepare_manual_refresh(&mut self) {
        let page = self.current_page;
        let Some(page_rows) = self.pages.get_mut(&page) else {
            return;
        };
        for row in Arc::make_mut(&mut page_rows.rows) {
            if let ProposalDocumentState::Resolved { document, .. } = &row.document
                && !document.available
            {
                row.document = ProposalDocumentState::Pending;
            }
        }
    }

    pub(super) fn replace_refreshed_page(
        &mut self,
        source_page: usize,
        destination_page: usize,
        mut rows: Vec<ResolvedProposal>,
    ) {
        let source_rows = self.pages.get(&source_page).map(|page| page.rows.clone());
        if let Some(source_rows) = source_rows {
            for row in &mut rows {
                let Some(source_row) = source_rows
                    .iter()
                    .find(|source_row| source_row.identity() == row.identity())
                else {
                    continue;
                };
                if let ProposalDocumentState::Resolved {
                    document,
                    presentation,
                } = &source_row.document
                    && document.available
                {
                    row.document = ProposalDocumentState::Resolved {
                        document: document.clone(),
                        presentation: presentation.clone(),
                    };
                }
            }
        }

        let selected = self.selected.take().and_then(|mut selected| {
            if selected.page == source_page
                && rows.iter().any(|row| row.identity() == selected.identity)
            {
                selected.page = destination_page;
                Some(selected)
            } else {
                None
            }
        });
        let source_list = self.proposal_lists.remove(&source_page);
        self.pages.clear();
        self.pages.insert(
            destination_page,
            ProposalsPage {
                rows: Arc::new(rows),
            },
        );
        self.current_page = destination_page;
        self.loading_pages.clear();
        self.hydrating_pages.clear();
        self.proposal_lists.clear();
        if let Some(source_list) = source_list {
            self.proposal_lists.insert(destination_page, source_list);
        }
        let prepared_tables = self.pages[&destination_page]
            .rows
            .iter()
            .filter_map(|row| {
                row.presentation().and_then(|presentation| {
                    let ProposalPresentation::Prepared(prepared) = presentation else {
                        return None;
                    };
                    Some((row.identity(), prepared.table_count))
                })
            })
            .collect::<Vec<_>>();
        for (identity, table_count) in prepared_tables {
            self.ensure_table_scroll_handles(&identity, table_count);
        }
        self.selected = selected;
        let valid_actions = self
            .pages
            .get(&destination_page)
            .map(|page| {
                page.rows
                    .iter()
                    .flat_map(|row| {
                        (0..row.proposal.actions.len()).map(move |ordinal| ProposalActionIdentity {
                            proposal: row.identity(),
                            ordinal,
                        })
                    })
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        self.expanded_calldata
            .retain(|key| valid_actions.contains(key));
    }

    pub(super) fn prefetch_candidate(&self, current: usize) -> Option<usize> {
        if current != self.current_page || !self.page_is_terminal(current) {
            return None;
        }
        next_proposal_page(current, self.total_pages)
    }
}
