use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::sync::{Semaphore, mpsc, oneshot};

use alloy::primitives::U256;
use chrono::{DateTime, Local, Utc};
use gpui::{
    AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, WeakEntity,
    Window, div, px, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable,
    alert::Alert,
    button::ButtonVariants,
    collapsible::Collapsible,
    divider::Divider,
    list::{List, ListDelegate, ListItem, ListState},
    scroll::ScrollableElement,
    skeleton::Skeleton,
    tab::{Tab, TabBar},
    text::TextView,
    tooltip::Tooltip,
};
use markdown::{ParseOptions, mdast::Node, to_mdast};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button, app_button_base, app_muted_text, app_strong_text, app_text};
use ui::format::format_compact_duration;
use ui::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::{
    GovernanceContractRules, GovernanceDocument, GovernanceOverview, GovernanceProposal,
    GovernanceProposalStage, GovernanceProposalStatus, HttpContext,
    derive_governance_proposal_status, fetch_governance_chain_time, fetch_governance_overview,
    fetch_governance_page, resolve_governance_document,
    settings::{EffectiveChainConfig, load_wallet_settings},
    vault::DesktopVaultStore,
};

use super::spend_authorization::spend_authorization_recipient_display;
use super::tokens::format_native_token_amount_for_display;
use super::{WalletRoot, app_refresh_button, app_status_tag, format_report_chain};
use crate::assets::RailgunSidebarIcon;

pub(super) const PROPOSALS_PAGE_SIZE: usize = 5;
const DOCUMENT_RESOLUTION_CONCURRENCY: usize = 4;
const CONTENT_WIDTH: gpui::Pixels = px(1080.0);
const GOVERNANCE_HEADER_HEIGHT: gpui::Pixels = px(52.0);
const MAX_MDAST_NODES: usize = 4_096;
const MAX_RENDER_COMPLEXITY: usize = 256;
const MAX_PREPARED_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_TABLE_COLUMN_LENGTH: usize = 5;
const MAX_TABLE_COLUMN_LENGTH: usize = 150;
const TABLE_COLUMN_CHROME_PX: usize = 17;
const TABLE_OUTER_BORDER_PX: usize = 2;
const MIN_TABLE_RENDER_WIDTH_PX: usize = 1;
const MAX_TABLE_RENDER_WIDTH_PX: usize = 4_096;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct ProposalIdentity {
    contract_version: wallet_ops::GovernanceContractVersion,
    contract_address: alloy::primitives::Address,
    index: U256,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ProposalDetailTab {
    #[default]
    Description,
    Actions,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ProposalActionIdentity {
    proposal: ProposalIdentity,
    ordinal: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProposalDocumentState {
    Pending,
    Resolved {
        document: GovernanceDocument,
        presentation: ProposalPresentation,
    },
}

impl ProposalDocumentState {
    const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

#[derive(Clone, Debug)]
pub(super) struct ResolvedProposal {
    pub proposal: GovernanceProposal,
    pub rules: GovernanceContractRules,
    document: ProposalDocumentState,
}

impl ResolvedProposal {
    const fn identity(&self) -> ProposalIdentity {
        ProposalIdentity {
            contract_version: self.proposal.contract_version,
            contract_address: self.proposal.contract_address,
            index: self.proposal.index,
        }
    }

    const fn document(&self) -> Option<&GovernanceDocument> {
        match &self.document {
            ProposalDocumentState::Pending => None,
            ProposalDocumentState::Resolved { document, .. } => Some(document),
        }
    }

    const fn presentation(&self) -> Option<&ProposalPresentation> {
        match &self.document {
            ProposalDocumentState::Pending => None,
            ProposalDocumentState::Resolved { presentation, .. } => Some(presentation),
        }
    }

    const fn has_pending_document(&self) -> bool {
        self.document.is_pending()
    }

    fn status(&self, chain_time: U256) -> GovernanceProposalStatus {
        derive_governance_proposal_status(&self.proposal, &self.rules, chain_time)
            .expect("validated governance rules must derive a status")
    }
}

#[derive(Clone, Debug)]
struct ChainTimeAnchor {
    chain_time: U256,
    captured_at: Instant,
}

impl ChainTimeAnchor {
    fn current(&self) -> U256 {
        self.current_at(self.captured_at.elapsed().as_secs())
    }

    fn current_at(&self, elapsed_secs: u64) -> U256 {
        advance_chain_time(self.chain_time, elapsed_secs)
    }
}

fn advance_chain_time(chain_time: U256, elapsed_secs: u64) -> U256 {
    chain_time
        .checked_add(U256::from(elapsed_secs))
        .unwrap_or(U256::MAX)
}

#[derive(Clone, Debug)]
struct ProposalsPage {
    rows: Arc<Vec<ResolvedProposal>>,
}

pub(super) struct ProposalsState {
    pub chain_id: u64,
    pub generation: u64,
    pub request_generation: u64,
    pub checked: bool,
    pub overview: Option<GovernanceOverview>,
    chain_time_anchor: Option<ChainTimeAnchor>,
    pub total_pages: usize,
    pub current_page: usize,
    pages: BTreeMap<usize, ProposalsPage>,
    loading_pages: BTreeSet<usize>,
    hydrating_pages: BTreeSet<usize>,
    prefetch_page: Option<usize>,
    active_page_token: u64,
    prefetch_token: u64,
    proposal_lists: BTreeMap<usize, Entity<ListState<ProposalListDelegate>>>,
    focus_list_on_render: bool,
    pub(super) table_scroll_handles: BTreeMap<String, ScrollHandle>,
    expanded_calldata: BTreeSet<ProposalActionIdentity>,
    detail_scroll_handle: ScrollHandle,
    pub loading: bool,
    pub refreshing: bool,
    pub error: Option<Arc<str>>,
    pub selected: Option<ProposalSelection>,
    time_tick_active: bool,
    task_tracker: ProposalTaskTracker,
    active_page_task_tracker: ProposalTaskTracker,
    prefetch_task_tracker: ProposalTaskTracker,
    document_semaphore: Arc<Semaphore>,
}

struct ProposalListDelegate {
    root: WeakEntity<WalletRoot>,
    selected: Option<gpui_component::IndexPath>,
}

impl ProposalListDelegate {
    const fn new(root: WeakEntity<WalletRoot>) -> Self {
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
            root.select_proposal(page, identity, window, cx);
        });
    }
}

#[derive(Clone, Debug)]
pub(super) struct ProposalSelection {
    page: usize,
    identity: ProposalIdentity,
    tab: ProposalDetailTab,
}

struct ProposalTaskTracker {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ProposalTaskTracker {
    fn track(&mut self, task: tokio::task::JoinHandle<()>) {
        self.tasks.retain(|task| !task.is_finished());
        self.tasks.push(task);
    }

    fn abort_all(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }

    fn abort_and_clear(&mut self) {
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

pub(super) struct ProposalCleanup {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ProposalCleanup {
    #[cfg(test)]
    pub(super) const fn empty() -> Self {
        Self { tasks: Vec::new() }
    }

    pub(super) async fn shutdown(self) -> Result<(), String> {
        for task in &self.tasks {
            task.abort();
        }
        let mut failures = 0;
        for task in self.tasks {
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                failures += 1;
                tracing::warn!(%error, "proposal worker failed during cleanup");
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
    pub(super) fn new(chain_id: u64) -> Self {
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
    fn invalidate(&mut self, chain_id: u64) {
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
    pub(super) fn take_cleanup(&mut self) -> ProposalCleanup {
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
    fn cancel_prefetch(&mut self) {
        self.prefetch_task_tracker.abort_and_clear();
        self.prefetch_token = self.prefetch_token.wrapping_add(1);
        if let Some(page) = self.prefetch_page.take() {
            self.loading_pages.remove(&page);
            self.hydrating_pages.remove(&page);
        }
    }
    fn cancel_active_page(&mut self, page: usize) {
        self.active_page_task_tracker.abort_and_clear();
        self.active_page_token = self.active_page_token.wrapping_add(1);
        self.loading_pages.remove(&page);
        self.hydrating_pages.remove(&page);
    }
    fn owns_page_work(&self, page: usize, prefetch: bool, token: u64) -> bool {
        if prefetch {
            self.prefetch_page == Some(page) && self.prefetch_token == token
        } else {
            self.current_page == page && self.active_page_token == token
        }
    }
    fn rows(&self, page: usize) -> Option<&[ResolvedProposal]> {
        self.pages.get(&page).map(|p| p.rows.as_slice())
    }

    fn chain_time(&self) -> U256 {
        self.chain_time_anchor
            .as_ref()
            .map_or(U256::ZERO, ChainTimeAnchor::current)
    }

    fn pending_document_rows(&self, page: usize) -> Option<Vec<ResolvedProposal>> {
        self.pages.get(&page).map(|page| {
            page.rows
                .iter()
                .filter(|row| row.has_pending_document())
                .cloned()
                .collect()
        })
    }

    fn page_is_terminal(&self, page: usize) -> bool {
        self.pages
            .get(&page)
            .is_some_and(|page| page.rows.iter().all(|row| !row.has_pending_document()))
    }

    fn apply_document(&mut self, page: usize, completion: DocumentCompletion) -> bool {
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

    fn ensure_table_scroll_handles(&mut self, identity: &ProposalIdentity, table_count: usize) {
        for ordinal in 0..table_count {
            self.table_scroll_handles
                .entry(proposal_table_scroll_key(identity, ordinal))
                .or_default();
        }
    }

    fn prepare_manual_refresh(&mut self) {
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

    fn replace_refreshed_page(
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

    fn prefetch_candidate(&self, current: usize) -> Option<usize> {
        if current != self.current_page || !self.page_is_terminal(current) {
            return None;
        }
        next_proposal_page(current, self.total_pages)
    }
}

struct InitialLoad {
    overview: Option<GovernanceOverview>,
    chain_time_anchor: Option<ChainTimeAnchor>,
    total_pages: usize,
    page: usize,
    rows: Vec<ResolvedProposal>,
}

impl WalletRoot {
    pub(super) fn open_proposals(&mut self, cx: &mut Context<'_, Self>) {
        self.active_activity = super::sidebar::Activity::Proposals;
        self.proposals.focus_list_on_render = true;
        if self.proposals.chain_time_anchor.is_some() {
            self.start_proposals_time_tick(cx);
        }
        if !self.proposals.checked && !self.proposals.loading {
            self.start_proposals_refresh(false, cx);
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
                    && root.active_activity == super::sidebar::Activity::Proposals
                {
                    cx.notify();
                    root.start_proposals_time_tick(cx);
                }
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn invalidate_proposals_chain(&mut self, chain_id: u64) {
        self.proposals.invalidate(chain_id);
        if self.active_activity == super::sidebar::Activity::Proposals {
            self.proposals.focus_list_on_render = true;
        }
    }
    pub(super) fn start_proposals_refresh(&mut self, manual: bool, cx: &mut Context<'_, Self>) {
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
        let chain_id = self.selected_chain;
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
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let (result_tx, result_rx) = oneshot::channel();
        let join = self.runtime.spawn(async move {
            let result =
                load_initial(chain_id, requested_page, effective_chain.as_ref(), &http).await;
            let _ = result_tx.send(result);
        });
        self.proposals.task_tracker.track(join);
        cx.spawn(async move |this, cx| {
            let Ok(result) = result_rx.await else {
                return;
            };
            let _ = this.update(cx, |root, cx| {
                if root.selected_chain != chain_id
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
                        if root.proposals.overview.is_some() {
                            root.start_proposals_time_tick(cx);
                            root.resume_active_proposals_page(load.page, cx);
                        }
                    }
                    Err(error) => {
                        root.proposals.error = Some(Arc::from(format_report_chain(&error)));
                        if root.proposals.chain_time_anchor.is_some()
                            && root.active_activity == super::sidebar::Activity::Proposals
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
    pub(super) fn select_proposals_page(&mut self, page: usize, cx: &mut Context<'_, Self>) {
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
        self.proposals.expanded_calldata.clear();
        self.proposals.focus_list_on_render = true;
        if self.proposals.rows(page).is_none() {
            self.request_proposals_page(page, false, cx);
        } else {
            self.resume_active_proposals_page(page, cx);
        }
        cx.notify();
    }
    pub(super) fn select_proposal(
        &mut self,
        page: usize,
        identity: ProposalIdentity,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let table_count =
            self.proposals
                .rows(page)
                .and_then(|rows| rows.iter().find(|row| row.identity() == identity))
                .and_then(ResolvedProposal::presentation)
                .and_then(|presentation| match presentation {
                    ProposalPresentation::Prepared(prepared) => Some(prepared.table_count),
                    ProposalPresentation::RawParseFallback(_)
                    | ProposalPresentation::TooComplex => None,
                });
        if let Some(table_count) = table_count {
            self.proposals
                .ensure_table_scroll_handles(&identity, table_count);
        }
        self.proposals.expanded_calldata.clear();
        self.proposals.selected = Some(ProposalSelection {
            page,
            identity,
            tab: ProposalDetailTab::default(),
        });
        self.proposals.detail_scroll_handle = ScrollHandle::new();
        self.proposal_detail_focus.focus(window);
        cx.notify();
    }
    pub(super) fn clear_selected_proposal(&mut self, cx: &mut Context<'_, Self>) {
        self.proposals.selected = None;
        self.proposals.expanded_calldata.clear();
        self.proposals.focus_list_on_render = true;
        cx.notify();
    }
    fn select_proposal_detail_tab(&mut self, tab: ProposalDetailTab, cx: &mut Context<'_, Self>) {
        if let Some(selection) = self.proposals.selected.as_mut() {
            selection.tab = tab;
            cx.notify();
        }
    }
    fn toggle_proposal_calldata(
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
    pub(super) fn retry_proposals(&mut self, cx: &mut Context<'_, Self>) {
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
        if self.proposals.pages.contains_key(&page) || !self.proposals.loading_pages.insert(page) {
            return;
        }
        let chain_id = self.selected_chain;
        let chain_generation = self.proposals.generation;
        let request_generation = self.proposals.request_generation;
        let work_token = if prefetch {
            self.proposals.prefetch_token
        } else {
            self.proposals.active_page_token
        };
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let http = self.http.clone();
        tracing::debug!(chain_id, page, prefetch, "loading governance proposal page");
        let (result_tx, result_rx) = oneshot::channel();
        let join = self.runtime.spawn(async move {
            let result = load_page(&overview, page, effective_chain.as_ref(), &http).await;
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
                if root.selected_chain != chain_id
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
        if !self.proposals.hydrating_pages.insert(page) {
            return;
        }
        let chain_id = self.selected_chain;
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
                    if root.selected_chain != chain_id
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
                if root.selected_chain != chain_id
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

    fn ensure_proposal_list(
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

    pub(super) fn render_proposals_view(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let active_detail = self.proposals.selected.as_ref().and_then(|selection| {
            self.proposals
                .rows(selection.page)
                .and_then(|rows| rows.iter().find(|row| row.identity() == selection.identity))
        });
        let has_active_detail = active_detail.is_some();
        let active_content = if let Some(proposal) = active_detail {
            self.render_proposal_detail(root, proposal, window, cx)
                .into_any_element()
        } else {
            self.render_proposal_list(root, window, cx)
                .into_any_element()
        };
        let keyboard_root = root.clone();
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .on_key_down(move |event: &KeyDownEvent, _window, cx| {
                if event.keystroke.key == "escape" && has_active_detail {
                    keyboard_root.update(cx, |root, cx| {
                        root.clear_selected_proposal(cx);
                    });
                    cx.stop_propagation();
                    return;
                }
                if has_active_detail || event.keystroke.modifiers.modified() {
                    return;
                }
                let is_left = match event.keystroke.key.as_str() {
                    "left" => true,
                    "right" => false,
                    _ => return,
                };
                keyboard_root.update(cx, |root, cx| {
                    let page = if is_left {
                        root.proposals.current_page.checked_sub(1)
                    } else {
                        next_proposal_page(root.proposals.current_page, root.proposals.total_pages)
                    };
                    if let Some(page) = page {
                        root.select_proposals_page(page, cx);
                    }
                });
                cx.stop_propagation();
            })
            .child(self.render_proposals_header(root))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .child(active_content),
            )
            .into_any_element()
    }
    fn render_proposal_list(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::AnyElement {
        let page = self.proposals.current_page;
        let row_count = self.proposals.rows(page).map_or(0, <[_]>::len);
        let list = if row_count > 0 {
            let list = self.ensure_proposal_list(page, root, window, cx);
            let focus_list_on_render = self.proposals.focus_list_on_render;
            list.update(cx, |list, cx| {
                if let Some(index) = list.selected_index()
                    && index.row >= row_count
                {
                    list.set_selected_index(
                        Some(gpui_component::IndexPath::default().row(row_count - 1)),
                        window,
                        cx,
                    );
                }
                if focus_list_on_render {
                    if list.selected_index().is_none() {
                        list.set_selected_index(
                            Some(gpui_component::IndexPath::default()),
                            window,
                            cx,
                        );
                    }
                    list.focus(window, cx);
                }
            });
            if focus_list_on_render {
                self.proposals.focus_list_on_render = false;
            }
            Some(list)
        } else {
            None
        };
        let state = &self.proposals;
        let mut content = div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED));
        let mut body = div()
            .w(CONTENT_WIDTH)
            .max_w_full()
            .mx_auto()
            .flex_1()
            .min_h(px(0.0))
            .p(px(16.0))
            .flex()
            .flex_col();
        let loading_without_rows =
            (state.loading || state.loading_pages.contains(&page)) && state.rows(page).is_none();
        let mut list_content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .gap_3()
            .child(governance_permissionless_warning());
        let total = state
            .overview
            .as_ref()
            .and_then(|overview| total_proposals(overview).ok());
        let show_toolbar = total.is_some() || state.total_pages > 1;
        if show_toolbar {
            let mut toolbar = div().w_full().flex().items_center().justify_between();
            if let Some(total) = total {
                toolbar =
                    toolbar.child(app_muted_text(format!("{total} proposals")).text_size(px(12.0)));
            }
            if state.total_pages > 1 {
                let previous_root = root.clone();
                let next_root = root.clone();
                toolbar = toolbar.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            app_button_base("wallet-proposals-previous")
                                .ghost()
                                .xsmall()
                                .compact()
                                .disabled(state.current_page == 0)
                                .icon(IconName::ChevronLeft)
                                .on_click(move |_event, _window, cx| {
                                    previous_root.update(cx, |root, cx| {
                                        let page = root.proposals.current_page.saturating_sub(1);
                                        root.select_proposals_page(page, cx);
                                    });
                                }),
                        )
                        .child(
                            app_muted_text(format!(
                                "{} / {}",
                                state.current_page + 1,
                                state.total_pages
                            ))
                            .text_size(px(12.0)),
                        )
                        .child(
                            app_button_base("wallet-proposals-next")
                                .ghost()
                                .xsmall()
                                .compact()
                                .disabled(state.current_page + 1 >= state.total_pages)
                                .icon(IconName::ChevronRight)
                                .on_click(move |_event, _window, cx| {
                                    next_root.update(cx, |root, cx| {
                                        let page = root.proposals.current_page + 1;
                                        root.select_proposals_page(page, cx);
                                    });
                                }),
                        ),
                );
            }
            list_content = list_content.child(toolbar);
        } else if loading_without_rows {
            list_content = list_content.child(div().h(px(16.0)).flex_none());
        }
        if loading_without_rows {
            let mut skeleton_rows = div().flex().flex_col().gap_3();
            for _ in 0..PROPOSALS_PAGE_SIZE {
                skeleton_rows = skeleton_rows.child(proposal_skeleton_row());
            }
            list_content = list_content.child(skeleton_rows);
        } else if state.overview.is_none() && state.checked && state.error.is_none() {
            list_content = list_content.child(proposals_empty_state(
                "Governance is not deployed on this chain.",
            ));
        } else if state.rows(page).is_some_and(<[_]>::is_empty) && state.error.is_none() {
            list_content =
                list_content.child(proposals_empty_state("No governance proposals found."));
        } else if let Some(list) = list {
            list_content = list_content.child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(List::new(&list).size_full()),
            );
        } else if state.error.is_some() {
            list_content =
                list_content.child(proposals_empty_state("Proposal data could not be loaded."));
        }
        if let Some(error) = state.error.as_ref() {
            let retry_root = root.clone();
            list_content = list_content.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Alert::error("wallet-proposals-error", error.to_string()).small())
                    .child(
                        app_button("wallet-proposals-retry", "Retry")
                            .small()
                            .on_click(move |_event, _window, cx| {
                                retry_root.update(cx, Self::retry_proposals);
                            }),
                    ),
            );
        }
        body = body.child(list_content);
        let body = if row_count > 0 {
            body.into_any_element()
        } else {
            body.overflow_y_scrollbar().into_any_element()
        };
        content = content.child(body);
        content.into_any_element()
    }
    fn render_proposals_header(&self, root: &Entity<Self>) -> gpui::Div {
        let refresh_root = root.clone();
        let state = &self.proposals;
        div()
            .h(GOVERNANCE_HEADER_HEIGHT)
            .flex_none()
            .flex()
            .items_center()
            .gap_3()
            .px(px(14.0))
            .bg(rgb(theme::SURFACE))
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .child(
                Icon::new(RailgunSidebarIcon::Landmark)
                    .size_5()
                    .text_color(rgb(theme::PRIMARY)),
            )
            .child(
                app_strong_text("Governance")
                    .text_size(px(20.0))
                    .font_weight(FontWeight::SEMIBOLD),
            )
            .child(div().flex_1())
            .child(app_refresh_button(
                "wallet-proposals-refresh",
                "Refresh governance proposals",
                state.refreshing,
                true,
                move |_window, cx| {
                    refresh_root.update(cx, |root, cx| {
                        root.start_proposals_refresh(true, cx);
                    });
                },
            ))
            .child(
                Divider::vertical()
                    .h(px(18.0))
                    .mx(px(2.0))
                    .color(rgb(theme::BORDER)),
            )
            .child(self.render_chain_selector())
    }
    fn render_proposal_row(
        proposal: &ResolvedProposal,
        chain_time: U256,
        selected: bool,
    ) -> ListItem {
        let status = proposal.status(chain_time);
        let title = proposal.document().map(|document| {
            if document.available {
                ProposalRowTitle::Available(document.title.clone())
            } else {
                ProposalRowTitle::Unavailable
            }
        });
        let identity = proposal.identity();
        let group_name = proposal_row_group(&identity);
        let row_id = SharedString::from(format!(
            "wallet-proposal-row-{}-{}-{}",
            proposal_version_label(proposal.proposal.contract_version),
            proposal.proposal.contract_address,
            proposal.proposal.index,
        ));
        let row = div()
            .group(group_name)
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .px(px(14.0))
            .py(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_muted_text(format!("#{}", proposal.proposal.index))
                            .font_family(APP_MONO_FONT_FAMILY)
                            .text_color(rgb(theme::TEXT_SUBTLE))
                            .text_size(px(12.0)),
                    )
                    .child(proposal_row_title(title))
                    .child(div().ml_auto().child(app_status_tag(
                        proposal_stage_label(status.stage),
                        proposal_stage_color(status.stage),
                    ))),
            )
            .child(proposal_row_meta(proposal, chain_time));
        let row = if selected {
            row.bg(rgb(theme::SURFACE))
        } else {
            row
        };
        ListItem::new(row_id)
            .p_0()
            .mb_3()
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(theme::SURFACE))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .child(row)
    }
    fn render_proposal_detail(
        &self,
        root: &Entity<Self>,
        proposal: &ResolvedProposal,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::AnyElement {
        let back_root = root.clone();
        let chain_time = self.proposals.chain_time();
        let status = proposal.status(chain_time);
        let detail_tab = self
            .proposals
            .selected
            .as_ref()
            .map_or_else(ProposalDetailTab::default, |selection| selection.tab);
        let proposer = proposal.proposal.proposer.to_checksum(None);
        let description_id = SharedString::from(format!(
            "wallet-proposal-description-{}-{}",
            proposal_version_label(proposal.proposal.contract_version),
            proposal.proposal.index,
        ));
        let detail_scroll = self.proposals.detail_scroll_handle.clone();
        let mut detail_scroller = div()
            .id("wallet-proposals-detail-scroll")
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .track_scroll(&detail_scroll)
            .overflow_y_scroll()
            .vertical_scrollbar(&detail_scroll)
            .relative();
        detail_scroller.style().restrict_scroll_to_axis = Some(true);
        detail_scroller
            .bg(rgb(theme::SURFACE_ELEVATED))
            .track_focus(&self.proposal_detail_focus)
            .child(
                div()
                    .w(CONTENT_WIDTH)
                    .max_w_full()
                    .mx_auto()
                    .p(px(16.0))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(governance_permissionless_warning())
                    .child(
                        div().flex().items_center().flex_wrap().gap_2().child(
                            div()
                                .flex()
                                .items_center()
                                .flex_none()
                                .gap_2()
                                .child(
                                    app_button_base("wallet-proposals-back")
                                        .ghost()
                                        .xsmall()
                                        .compact()
                                        .icon(IconName::ArrowLeft)
                                        .tooltip("Back to governance")
                                        .on_click(move |_event, _window, cx| {
                                            back_root.update(cx, |root, cx| {
                                                root.clear_selected_proposal(cx);
                                            });
                                        }),
                                )
                                .child(
                                    app_muted_text(format!(
                                        "Proposal #{}",
                                        proposal.proposal.index
                                    ))
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(px(12.0)),
                                )
                                .child(app_status_tag(
                                    proposal_stage_label(status.stage),
                                    proposal_stage_color(status.stage),
                                ))
                                .children(
                                    (proposal.proposal.contract_version
                                        == wallet_ops::GovernanceContractVersion::V1)
                                        .then(|| app_status_tag("V1", theme::TEXT_MUTED)),
                                ),
                        ),
                    )
                    .child(proposal_detail_title(proposal))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(app_muted_text("Proposed by").text_size(px(12.0)))
                                    .child(
                                        app_text(spend_authorization_recipient_display(&proposer))
                                            .font_family(APP_MONO_FONT_FAMILY)
                                            .text_size(px(12.0)),
                                    )
                                    .child(
                                        div()
                                            .id(proposal_copy_id(
                                                proposal,
                                                "proposer-meta",
                                                "action",
                                            ))
                                            .tooltip(|window, cx| {
                                                Tooltip::new("Copy Proposer").build(window, cx)
                                            })
                                            .child(clipboard_with_toast(
                                                proposal_copy_id(
                                                    proposal,
                                                    "proposer-meta",
                                                    "clipboard",
                                                ),
                                                proposer.clone(),
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .flex_none()
                                    .child(app_muted_text("·").text_size(px(12.0)))
                                    .child(
                                        app_muted_text(format!(
                                            "Published {}",
                                            format_datetime_short(&proposal.proposal.publish_time)
                                        ))
                                        .text_size(px(12.0)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_4()
                            .items_start()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(render_proposal_detail_tabs(
                                        root,
                                        detail_tab,
                                        proposal.proposal.actions.len(),
                                        &proposal.identity(),
                                    ))
                                    .child(match detail_tab {
                                        ProposalDetailTab::Description => proposal_document_card(
                                            proposal,
                                            description_id,
                                            &self.proposals.table_scroll_handles,
                                            window,
                                            cx,
                                        )
                                        .into_any_element(),
                                        ProposalDetailTab::Actions => render_proposal_actions_card(
                                            root,
                                            proposal,
                                            self.proposals.chain_id,
                                            &self.proposals.expanded_calldata,
                                        )
                                        .into_any_element(),
                                    }),
                            )
                            .child(
                                div()
                                    .w(px(300.0))
                                    .flex_none()
                                    .pt(px(44.0))
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .children(match status.stage {
                                        GovernanceProposalStage::AwaitingSponsorship
                                        | GovernanceProposalStage::ReadyToCallVote
                                        | GovernanceProposalStage::SponsorshipExpired
                                        | GovernanceProposalStage::VoteCallExpired => {
                                            vec![render_sponsorship_card(proposal, chain_time)]
                                        }
                                        _ => vec![render_votes_card(proposal, chain_time)],
                                    })
                                    .child(render_timeline_card(proposal, chain_time))
                                    .child(render_details_card(proposal)),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn proposal_copy_control(
    proposal: &ResolvedProposal,
    label: &'static str,
    value: String,
    field: &'static str,
) -> gpui::Div {
    let display_value = spend_authorization_recipient_display(&value);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text(label).text_size(px(11.0)))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .mt(px(1.0))
                .child(
                    app_text(display_value)
                        .flex_1()
                        .min_w(px(0.0))
                        .text_color(rgb(theme::TEXT))
                        .font_family(APP_MONO_FONT_FAMILY)
                        .text_size(px(12.0)),
                )
                .child(
                    div()
                        .id(proposal_copy_id(proposal, field, "action"))
                        .flex_none()
                        .tooltip(move |window, cx| {
                            Tooltip::new(format!("Copy {label}")).build(window, cx)
                        })
                        .child(clipboard_with_toast(
                            proposal_copy_id(proposal, field, "clipboard"),
                            value,
                        )),
                ),
        )
}

async fn load_initial(
    chain_id: u64,
    requested_page: usize,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> eyre::Result<InitialLoad> {
    let overview = fetch_governance_overview(chain_id, effective_chain, http).await?;
    let Some(overview) = overview else {
        return Ok(InitialLoad {
            overview: None,
            chain_time_anchor: None,
            total_pages: 0,
            page: 0,
            rows: Vec::new(),
        });
    };
    let chain_time = fetch_governance_chain_time(overview.chain_id, effective_chain, http).await?;
    let chain_time_anchor = ChainTimeAnchor {
        chain_time,
        captured_at: Instant::now(),
    };
    let total = total_proposals(&overview)?;
    let total_pages = total.div_ceil(PROPOSALS_PAGE_SIZE);
    let page = if total_pages == 0 {
        0
    } else {
        requested_page.min(total_pages - 1)
    };
    let rows = if total == 0 {
        Vec::new()
    } else {
        load_page(&overview, page, effective_chain, http).await?
    };
    Ok(InitialLoad {
        overview: Some(overview),
        chain_time_anchor: Some(chain_time_anchor),
        total_pages,
        page,
        rows,
    })
}
async fn load_page(
    overview: &GovernanceOverview,
    page: usize,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> eyre::Result<Vec<ResolvedProposal>> {
    let size = std::num::NonZeroUsize::new(PROPOSALS_PAGE_SIZE).expect("non-zero page size");
    let proposals = fetch_governance_page(overview, page, size, effective_chain, http).await?;
    let mut rows = Vec::with_capacity(proposals.len());
    for proposal in proposals {
        let rules = rules_for(overview, proposal.contract_version)?;
        rows.push(ResolvedProposal {
            proposal,
            rules,
            document: ProposalDocumentState::Pending,
        });
    }
    Ok(rows)
}

struct DocumentCompletion {
    identity: ProposalIdentity,
    document: GovernanceDocument,
    presentation: ProposalPresentation,
}

async fn hydrate_page(
    rows: Vec<ResolvedProposal>,
    store: Option<&Arc<DesktopVaultStore>>,
    http: &HttpContext,
    semaphore: Arc<Semaphore>,
    completion_tx: mpsc::UnboundedSender<DocumentCompletion>,
) {
    let Some(db) = store.map(|store| store.db()) else {
        for row in rows {
            let _ = completion_tx.send(DocumentCompletion {
                identity: row.identity(),
                document: unavailable_document(),
                presentation: ProposalPresentation::RawParseFallback(inert_raw_fallback_source(
                    &unavailable_document().description,
                )),
            });
        }
        return;
    };
    let gateways = load_wallet_settings(db.as_ref())
        .ok()
        .map(|settings| settings.poi.artifact.gateway_urls)
        .unwrap_or_default();
    let futures = FuturesUnordered::new();
    for row in rows {
        let identity = row.identity();
        let cid = row.proposal.proposal_document.clone();
        let db = Arc::clone(&db);
        let http = http.clone();
        let gateways = gateways.clone();
        let semaphore = Arc::clone(&semaphore);
        futures.push(async move {
            let document = match semaphore.acquire_owned().await {
                Ok(permit) => {
                    let document = resolve_governance_document(&db, &http, &cid, &gateways).await;
                    drop(permit);
                    document
                }
                Err(_) => unavailable_document(),
            };
            let presentation = if document.available {
                let source = document.description.clone();
                let fallback_source = source.clone();
                match tokio::task::spawn_blocking(move || prepare_proposal_presentation(&source))
                    .await
                {
                    Ok(presentation) => presentation,
                    Err(_) => ProposalPresentation::RawParseFallback(inert_raw_fallback_source(
                        &fallback_source,
                    )),
                }
            } else {
                ProposalPresentation::RawParseFallback(inert_raw_fallback_source(
                    &document.description,
                ))
            };
            DocumentCompletion {
                identity,
                document,
                presentation,
            }
        });
    }
    send_document_completions(futures, completion_tx).await;
}

async fn send_document_completions<F>(
    mut futures: FuturesUnordered<F>,
    completion_tx: mpsc::UnboundedSender<DocumentCompletion>,
) where
    F: Future<Output = DocumentCompletion>,
{
    while let Some(completion) = futures.next().await {
        let _ = completion_tx.send(completion);
    }
}
fn unavailable_document() -> GovernanceDocument {
    GovernanceDocument {
        title: "Document unavailable".to_string(),
        description: String::new(),
        available: false,
    }
}
fn total_proposals(overview: &GovernanceOverview) -> eyre::Result<usize> {
    let v2 = usize::try_from(overview.v2.proposal_count)
        .map_err(|_| eyre::eyre!("governance V2 proposal count is too large"))?;
    let v1 = overview
        .v1
        .as_ref()
        .map_or(Ok(0), |s| usize::try_from(s.proposal_count))
        .map_err(|_| eyre::eyre!("governance V1 proposal count is too large"))?;
    v2.checked_add(v1)
        .ok_or_else(|| eyre::eyre!("governance proposal count overflows platform limits"))
}
fn rules_for(
    overview: &GovernanceOverview,
    version: wallet_ops::GovernanceContractVersion,
) -> eyre::Result<GovernanceContractRules> {
    Ok(match version {
        wallet_ops::GovernanceContractVersion::V2 => overview.v2.rules.clone(),
        wallet_ops::GovernanceContractVersion::V1 => overview
            .v1
            .as_ref()
            .ok_or_else(|| eyre::eyre!("governance V1 rules are unavailable"))?
            .rules
            .clone(),
    })
}
const fn proposal_stage_label(stage: GovernanceProposalStage) -> &'static str {
    match stage {
        GovernanceProposalStage::AwaitingSponsorship => "Awaiting sponsorship",
        GovernanceProposalStage::ReadyToCallVote => "Ready to call vote",
        GovernanceProposalStage::SponsorshipExpired => "Sponsorship expired",
        GovernanceProposalStage::VoteCallExpired => "Vote call expired",
        GovernanceProposalStage::VotingDelay => "Voting delay",
        GovernanceProposalStage::VotingOpen => "Voting open",
        GovernanceProposalStage::NayOnlyVoting => "Nay-only voting",
        GovernanceProposalStage::Failed => "Failed",
        GovernanceProposalStage::PassedAwaitingExecution => "Passed · awaiting execution",
        GovernanceProposalStage::PassedExecutable => "Passed · executable",
        GovernanceProposalStage::ExecutionExpired => "Execution expired",
        GovernanceProposalStage::Executed => "Executed",
    }
}
const fn proposal_stage_color(stage: GovernanceProposalStage) -> u32 {
    match stage {
        GovernanceProposalStage::AwaitingSponsorship
        | GovernanceProposalStage::ReadyToCallVote
        | GovernanceProposalStage::VotingDelay => theme::WARNING,
        GovernanceProposalStage::VotingOpen | GovernanceProposalStage::NayOnlyVoting => theme::BLUE,
        GovernanceProposalStage::PassedAwaitingExecution
        | GovernanceProposalStage::PassedExecutable
        | GovernanceProposalStage::Executed => theme::SUCCESS,
        GovernanceProposalStage::Failed => theme::DANGER,
        GovernanceProposalStage::SponsorshipExpired
        | GovernanceProposalStage::VoteCallExpired
        | GovernanceProposalStage::ExecutionExpired => theme::TEXT_MUTED,
    }
}
const fn proposal_version_label(version: wallet_ops::GovernanceContractVersion) -> &'static str {
    match version {
        wallet_ops::GovernanceContractVersion::V2 => "V2",
        wallet_ops::GovernanceContractVersion::V1 => "V1",
    }
}
fn next_proposal_page(current: usize, total_pages: usize) -> Option<usize> {
    current.checked_add(1).filter(|page| *page < total_pages)
}
fn governance_permissionless_warning() -> Alert {
    Alert::warning(
        "wallet-governance-permissionless-warning",
        "Governance is permissionless. Proposals may contain misleading or malicious content. Exercise caution, verify claims independently, and do your own research before taking action.",
    )
    .small()
}
fn proposals_empty_state(message: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .p(px(48.0))
        .child(
            Icon::new(IconName::Inbox)
                .size_6()
                .text_color(rgb(theme::TEXT_MUTED)),
        )
        .child(app_muted_text(message))
}
fn proposal_copy_id(proposal: &ResolvedProposal, field: &str, control: &str) -> SharedString {
    SharedString::from(format!(
        "wallet-proposal-{}-{}-{field}-{control}",
        proposal_version_label(proposal.proposal.contract_version),
        proposal.proposal.index,
    ))
}
fn proposal_table_scroll_key(identity: &ProposalIdentity, ordinal: usize) -> String {
    format!(
        "wallet-proposal-table-scroll-v1-{}-{}-{}-{ordinal}",
        proposal_version_label(identity.contract_version),
        identity.contract_address,
        identity.index,
    )
}
fn proposal_row_group(identity: &ProposalIdentity) -> SharedString {
    SharedString::from(format!(
        "wallet-proposal-row-group-{}-{}-{}",
        proposal_version_label(identity.contract_version),
        identity.contract_address,
        identity.index,
    ))
}
fn format_deadline(deadline: &U256) -> String {
    format_datetime_short(deadline)
}
fn format_date_short(timestamp: &U256) -> String {
    format_timestamp_with_pattern(timestamp, "%d %b %Y")
}
fn format_datetime_short(timestamp: &U256) -> String {
    format_timestamp_with_pattern(timestamp, "%d %b %Y, %H:%M")
}
fn format_timestamp_with_pattern(timestamp: &U256, pattern: &str) -> String {
    let Ok(seconds) = i64::try_from(*timestamp) else {
        return format!("Unix timestamp {timestamp}");
    };
    let Some(utc) = DateTime::<Utc>::from_timestamp(seconds, 0) else {
        return format!("Unix timestamp {timestamp}");
    };
    utc.with_timezone(&Local).format(pattern).to_string()
}

fn format_compact_rail_amount(amount: U256) -> String {
    let scale = U256::from(10).pow(U256::from(18));
    let whole = amount / scale;
    if whole < U256::from(1_000) {
        return railgun_ui::format_token_amount(amount, 18);
    }
    let whole = u128::try_from(whole).unwrap_or(u128::MAX);
    if whole < 1_000_000 {
        let integer = whole / 1_000;
        let decimal = (whole % 1_000) / 100;
        return compact_suffix(integer, decimal, 'K');
    }
    if whole < 1_000_000_000 {
        let integer = whole / 1_000_000;
        let decimal = (whole % 1_000_000) / 10_000;
        return compact_suffix_two_decimals(integer, decimal, 'M');
    }
    let integer = whole / 1_000_000_000;
    let decimal = (whole % 1_000_000_000) / 10_000_000;
    compact_suffix_two_decimals(integer, decimal, 'B')
}
fn format_compact_rail_amount_with_unit(amount: U256) -> String {
    format!("{} RAIL", format_compact_rail_amount(amount))
}
fn compact_suffix(integer: u128, decimal: u128, suffix: char) -> String {
    if decimal == 0 {
        format!("{integer}{suffix}")
    } else {
        format!("{integer}.{decimal}{suffix}")
    }
}
fn compact_suffix_two_decimals(integer: u128, decimal: u128, suffix: char) -> String {
    if decimal == 0 {
        format!("{integer}{suffix}")
    } else if decimal.is_multiple_of(10) {
        format!("{integer}.{}{}", decimal / 10, suffix)
    } else {
        format!("{integer}.{decimal:02}{suffix}")
    }
}
fn per_mille(numerator: U256, denominator: U256) -> u16 {
    if denominator.is_zero() {
        return 0;
    }
    let numerator = u128::try_from(numerator).unwrap_or(u128::MAX);
    let denominator = u128::try_from(denominator).unwrap_or(u128::MAX);
    u16::try_from((numerator.saturating_mul(1_000) / denominator).min(1_000)).unwrap_or(1_000)
}
fn vote_split(yay: U256, nay: U256) -> Option<u16> {
    let total = yay.checked_add(nay)?;
    if total.is_zero() {
        return None;
    }
    Some(per_mille(yay, total))
}
fn format_ratio_percent(per_mille: u16) -> String {
    format!("{}.{:01}%", per_mille / 10, per_mille % 10)
}
fn ratio_fraction(per_mille: u16) -> f32 {
    f32::from(per_mille) / 1_000.0
}
fn countdown(deadline: &U256, now: U256) -> Option<String> {
    let remaining = deadline.checked_sub(now)?;
    let remaining = u64::try_from(remaining).ok()?;
    Some(format_compact_duration(Duration::from_secs(remaining)))
}
fn list_voting_deadline(status: &GovernanceProposalStatus) -> Option<(U256, &'static str)> {
    match status.stage {
        GovernanceProposalStage::VotingOpen => {
            Some((*status.deadlines.yay_end.as_ref()?, "Yay voting ends in"))
        }
        GovernanceProposalStage::NayOnlyVoting => {
            Some((*status.deadlines.nay_end.as_ref()?, "Nay voting ends in"))
        }
        _ => None,
    }
}
enum ProposalRowTitle {
    Available(String),
    Unavailable,
}
fn proposal_row_title(title: Option<ProposalRowTitle>) -> gpui::Div {
    match title {
        Some(ProposalRowTitle::Available(title)) if !title.is_empty() => app_strong_text(title)
            .flex_1()
            .min_w(px(0.0))
            .text_size(px(15.0))
            .font_weight(FontWeight::SEMIBOLD)
            .truncate(),
        Some(ProposalRowTitle::Available(_) | ProposalRowTitle::Unavailable) => {
            app_muted_text("Document unavailable")
                .flex_1()
                .min_w(px(0.0))
                .italic()
        }
        None => div()
            .flex_1()
            .min_w(px(0.0))
            .child(Skeleton::new().h(px(14.0)).w(px(180.0))),
    }
}
fn proposal_row_meta(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let mut left = div()
        .flex()
        .items_center()
        .gap_1()
        .flex_1()
        .min_w(px(0.0))
        .child(
            app_muted_text(format!(
                "Published {}",
                format_date_short(&proposal.proposal.publish_time)
            ))
            .text_size(px(12.0)),
        );
    if let Some(remaining) = match status.stage {
        GovernanceProposalStage::AwaitingSponsorship => {
            countdown(&status.deadlines.sponsorship, chain_time)
                .map(|remaining| format!(" · Sponsorship ends in {remaining}"))
        }
        GovernanceProposalStage::VotingOpen | GovernanceProposalStage::NayOnlyVoting => {
            list_voting_deadline(&status).and_then(|(deadline, label)| {
                countdown(&deadline, chain_time).map(|remaining| format!(" · {label} {remaining}"))
            })
        }
        GovernanceProposalStage::VotingDelay => countdown(
            &status
                .deadlines
                .voting_start
                .expect("called proposal deadline"),
            chain_time,
        )
        .map(|remaining| format!(" · Voting opens in {remaining}")),
        GovernanceProposalStage::PassedAwaitingExecution => countdown(
            &status
                .deadlines
                .execution_start
                .expect("called proposal deadline"),
            chain_time,
        )
        .map(|remaining| format!(" · Execution opens in {remaining}")),
        GovernanceProposalStage::PassedExecutable => countdown(
            &status
                .deadlines
                .execution_end
                .expect("called proposal deadline"),
            chain_time,
        )
        .map(|remaining| format!(" · Execution closes in {remaining}")),
        _ => None,
    } {
        left = left.child(app_muted_text(remaining).text_size(px(12.0)));
    }
    if proposal.proposal.contract_version == wallet_ops::GovernanceContractVersion::V1 {
        left = left.child(app_status_tag("V1", theme::TEXT_MUTED));
    }
    div()
        .flex()
        .items_center()
        .gap_2()
        .mt(px(4.0))
        .child(left)
        .child(render_list_meter(proposal, chain_time))
}
fn render_list_meter(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let group_name = proposal_row_group(&proposal.identity());
    let (for_amount, against_amount, caption, for_per_mille, color) = match status.stage {
        GovernanceProposalStage::AwaitingSponsorship
        | GovernanceProposalStage::ReadyToCallVote
        | GovernanceProposalStage::SponsorshipExpired
        | GovernanceProposalStage::VoteCallExpired => {
            let per_mille = sponsorship_per_mille(proposal);
            let percent = (u32::from(per_mille) + 5) / 10;
            (
                None,
                None,
                format!(
                    "{} / {} RAIL sponsored · {percent}%",
                    format_compact_rail_amount(proposal.proposal.sponsorship),
                    format_compact_rail_amount(proposal.rules.sponsor_threshold)
                ),
                per_mille,
                if matches!(
                    status.stage,
                    GovernanceProposalStage::SponsorshipExpired
                        | GovernanceProposalStage::VoteCallExpired
                ) {
                    theme::TEXT_MUTED
                } else {
                    theme::PRIMARY
                },
            )
        }
        _ => {
            let split = vote_split(proposal.proposal.yay_votes, proposal.proposal.nay_votes);
            let caption = split.map_or_else(
                || "No votes".to_string(),
                |_| {
                    format!(
                        "{} for · {} against",
                        format_compact_rail_amount(proposal.proposal.yay_votes),
                        format_compact_rail_amount(proposal.proposal.nay_votes)
                    )
                },
            );
            (
                Some(proposal.proposal.yay_votes),
                Some(proposal.proposal.nay_votes),
                caption,
                split.unwrap_or(0),
                theme::PRIMARY,
            )
        }
    };
    let track = div()
        .w(px(230.0))
        .h(px(6.0))
        .flex()
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(theme::SURFACE_HOVER))
        .child(
            if let (Some(yay), Some(nay)) = (for_amount, against_amount)
                && let Some(split) = vote_split(yay, nay)
            {
                let total_width = 230.0;
                let gap = if split > 0 && split < 1_000 { 2.0 } else { 0.0 };
                let split = ratio_fraction(split);
                div()
                    .flex()
                    .w(px(total_width))
                    .h(px(6.0))
                    .child(
                        div()
                            .w(px((total_width - gap) * split))
                            .h(px(6.0))
                            .bg(rgb(theme::PRIMARY)),
                    )
                    .child(
                        div()
                            .w(px(gap))
                            .h(px(6.0))
                            .bg(rgb(theme::SURFACE))
                            .group_hover(group_name, |style| {
                                style.bg(rgb(theme::SURFACE_HOVER_SUBTLE))
                            }),
                    )
                    .child(div().flex_1().h(px(6.0)).bg(rgb(theme::DANGER)))
            } else {
                div()
                    .w(px((230.0 * ratio_fraction(for_per_mille)).clamp(0.0, 230.0)))
                    .h(px(6.0))
                    .bg(rgb(color))
            },
        );
    div()
        .w(px(230.0))
        .flex()
        .flex_col()
        .items_end()
        .gap_1()
        .child(track)
        .child(app_muted_text(caption).text_size(px(11.0)))
}
fn proposal_skeleton_row() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .h(px(72.0))
        .gap_3()
        .px(px(14.0))
        .py(px(12.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(Skeleton::new().h(px(14.0)).w(px(240.0)))
        .child(Skeleton::new().secondary().h(px(10.0)).w(px(160.0)))
}
fn proposal_detail_title(proposal: &ResolvedProposal) -> gpui::Div {
    match proposal.document() {
        None => div().child(Skeleton::new().h(px(22.0)).w(px(360.0))),
        Some(document) if document.available && !document.title.is_empty() => {
            app_strong_text(document.title.clone())
                .text_size(px(22.0))
                .font_weight(FontWeight::SEMIBOLD)
                .whitespace_normal()
        }
        Some(_) => app_strong_text(format!("Proposal #{}", proposal.proposal.index))
            .text_size(px(22.0))
            .font_weight(FontWeight::SEMIBOLD),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProposalBlock {
    Markdown(String),
    Table(ProposalTable),
    List(ProposalList),
    Blockquote(ProposalBlockquote),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreparedProposal {
    blocks: Vec<ProposalBlock>,
    table_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProposalPresentation {
    Prepared(Arc<PreparedProposal>),
    RawParseFallback(String),
    TooComplex,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProposalPreparationFailure {
    RawParseFallback,
    TooComplex,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProposalBlockquote {
    blocks: Vec<ProposalBlock>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProposalTable {
    source: String,
    render_source: String,
    ordinal: usize,
    render_width_px: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProposalList {
    ordered: bool,
    start: Option<u32>,
    items: Vec<ProposalListItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProposalListItem {
    checked: Option<bool>,
    blocks: Vec<ProposalBlock>,
}

fn proposal_source_slice(
    source: &str,
    position: Option<&markdown::unist::Position>,
) -> Option<String> {
    let position = position?;
    source
        .get(position.start.offset..position.end.offset)
        .map(str::to_owned)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProposalSourceEdit {
    start: usize,
    end: usize,
    replacement: String,
}

fn max_backtick_run(value: &str) -> usize {
    let mut longest_run = 0;
    let mut run: usize = 0;
    for character in value.chars() {
        if character == '`' {
            run = run.saturating_add(1);
            longest_run = longest_run.max(run);
        } else {
            run = 0;
        }
    }
    longest_run
}

fn inert_code_span(value: &str) -> String {
    let longest_run = max_backtick_run(value);
    let fence = "`".repeat(longest_run.saturating_add(1).max(1));
    format!("{fence}{value}{fence}")
}

fn inert_raw_fallback_source(source: &str) -> String {
    let fence = "`".repeat(max_backtick_run(source).saturating_add(1).max(3));
    format!("{fence}\n{source}\n{fence}")
}

fn inert_text_source(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_string()
            } else {
                format!("&#{};", u32::from(character))
            }
        })
        .collect()
}

fn url_shaped_label(value: &str) -> bool {
    value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("mailto:")
        || value.starts_with("xmpp:")
        || value.starts_with("www.")
        || (value.contains('@') && !value.chars().any(char::is_whitespace))
}

fn visible_node_text(node: &Node) -> String {
    match node {
        Node::Image(image) => image.alt.clone(),
        Node::ImageReference(image) => image.alt.clone(),
        Node::Break(_) => "\n".to_owned(),
        _ => node.children().map_or_else(
            || node.to_string(),
            |children| children.iter().map(visible_node_text).collect(),
        ),
    }
}

fn bare_email_label(value: &str) -> bool {
    value.contains('@') && !value.contains(':') && !value.chars().any(char::is_whitespace)
}

fn label_represents_destination(label: &str, destination: &str) -> bool {
    (label == destination && url_shaped_label(label))
        || (bare_email_label(label)
            && destination
                .strip_prefix("mailto:")
                .is_some_and(|address| address == label))
}

fn inert_equivalent_label_source(
    source: &str,
    node: &Node,
    definitions: &BTreeMap<String, String>,
) -> Option<String> {
    match node {
        Node::Text(text) => Some(inert_text_source(&text.value)),
        Node::Link(_)
        | Node::LinkReference(_)
        | Node::Image(_)
        | Node::ImageReference(_)
        | Node::Html(_) => sanitized_node_source(source, node, definitions),
        _ => {
            let position = node.position()?;
            let original = source.get(position.start.offset..position.end.offset)?;
            let Some(children) = node.children() else {
                return Some(original.to_owned());
            };
            let mut rendered = String::with_capacity(original.len());
            let mut cursor = position.start.offset;
            for child in children {
                let child_position = child.position()?;
                if child_position.start.offset < cursor
                    || child_position.end.offset > position.end.offset
                {
                    return None;
                }
                rendered.push_str(source.get(cursor..child_position.start.offset)?);
                rendered.push_str(&inert_equivalent_label_source(source, child, definitions)?);
                cursor = child_position.end.offset;
            }
            rendered.push_str(source.get(cursor..position.end.offset)?);
            Some(rendered)
        }
    }
}

fn child_discloses_destination(
    node: &Node,
    destination: &str,
    definitions: &BTreeMap<String, String>,
) -> bool {
    match node {
        Node::Image(image) => image.url == destination,
        Node::ImageReference(image) => definitions
            .get(&image.identifier)
            .is_some_and(|image_destination| image_destination == destination),
        _ => node.children().is_some_and(|children| {
            children
                .iter()
                .any(|child| child_discloses_destination(child, destination, definitions))
        }),
    }
}

fn sanitized_image_source(alt: &str, destination: &str) -> String {
    if label_represents_destination(alt, destination) {
        inert_text_source(alt)
    } else {
        format!(
            "{} ({})",
            inert_text_source(alt),
            inert_text_source(destination)
        )
    }
}

fn sanitized_link_source(
    source: &str,
    children: &[Node],
    destination: &str,
    definitions: &BTreeMap<String, String>,
) -> Option<String> {
    let visible_label = children.iter().map(visible_node_text).collect::<String>();
    let represents_destination = label_represents_destination(&visible_label, destination);
    let mut rendered = String::new();
    for child in children {
        let child_source = if represents_destination {
            inert_equivalent_label_source(source, child, definitions)?
        } else {
            sanitized_node_source(source, child, definitions)?
        };
        rendered.push_str(&child_source);
    }
    if !represents_destination
        && !children
            .iter()
            .any(|child| child_discloses_destination(child, destination, definitions))
    {
        rendered.push_str(" (");
        rendered.push_str(&inert_text_source(destination));
        rendered.push(')');
    }
    Some(rendered)
}

fn sanitized_node_source(
    source: &str,
    node: &Node,
    definitions: &BTreeMap<String, String>,
) -> Option<String> {
    match node {
        Node::Link(link) => sanitized_link_source(source, &link.children, &link.url, definitions),
        Node::LinkReference(reference) => {
            let destination = definitions.get(&reference.identifier)?;
            sanitized_link_source(source, &reference.children, destination, definitions)
        }
        Node::Image(image) => Some(sanitized_image_source(&image.alt, &image.url)),
        Node::ImageReference(image) => {
            let destination = definitions.get(&image.identifier)?;
            Some(sanitized_image_source(&image.alt, destination))
        }
        Node::Html(html) => Some(inert_code_span(&html.value)),
        _ => {
            let position = node.position()?;
            let original = source.get(position.start.offset..position.end.offset)?;
            let Some(children) = node.children() else {
                return Some(original.to_owned());
            };
            let mut rendered = String::with_capacity(original.len());
            let mut cursor = position.start.offset;
            for child in children {
                let child_position = child.position()?;
                if child_position.start.offset < cursor
                    || child_position.end.offset > position.end.offset
                {
                    return None;
                }
                rendered.push_str(source.get(cursor..child_position.start.offset)?);
                rendered.push_str(&sanitized_node_source(source, child, definitions)?);
                cursor = child_position.end.offset;
            }
            rendered.push_str(source.get(cursor..position.end.offset)?);
            Some(rendered)
        }
    }
}

fn collect_sanitizer_edits(
    source: &str,
    nodes: &[Node],
    definitions: &BTreeMap<String, String>,
    edits: &mut Vec<ProposalSourceEdit>,
) -> Option<()> {
    for node in nodes {
        let target = matches!(
            node,
            Node::Link(_)
                | Node::LinkReference(_)
                | Node::Image(_)
                | Node::ImageReference(_)
                | Node::Html(_)
        );
        if target {
            let position = node.position()?;
            let replacement = sanitized_node_source(source, node, definitions)?;
            source.get(position.start.offset..position.end.offset)?;
            edits.push(ProposalSourceEdit {
                start: position.start.offset,
                end: position.end.offset,
                replacement,
            });
        } else if let Some(children) = node.children() {
            collect_sanitizer_edits(source, children, definitions, edits)?;
        }
    }
    Some(())
}

fn apply_source_edits(source: &str, mut edits: Vec<ProposalSourceEdit>) -> Option<String> {
    edits.sort_by_key(|edit| (edit.start, edit.end));
    let mut rendered = String::with_capacity(source.len());
    let mut cursor = 0;
    for edit in edits {
        if edit.start < cursor
            || edit.start > edit.end
            || source.get(edit.start..edit.end).is_none()
        {
            return None;
        }
        rendered.push_str(source.get(cursor..edit.start)?);
        rendered.push_str(&edit.replacement);
        cursor = edit.end;
    }
    rendered.push_str(source.get(cursor..)?);
    Some(rendered)
}

fn contains_activatable_markdown(nodes: &[Node]) -> bool {
    nodes.iter().any(|node| {
        matches!(
            node,
            Node::Link(_)
                | Node::LinkReference(_)
                | Node::Image(_)
                | Node::ImageReference(_)
                | Node::Html(_)
        ) || node
            .children()
            .is_some_and(|children| contains_activatable_markdown(children))
    })
}

fn sanitize_render_source(source: &str) -> Result<String, ProposalPreparationFailure> {
    let Node::Root(root) = to_mdast(source, &ParseOptions::gfm())
        .map_err(|_| ProposalPreparationFailure::RawParseFallback)?
    else {
        return Err(ProposalPreparationFailure::RawParseFallback);
    };
    let definitions = collect_effective_definition_urls(&root.children);
    let mut edits = Vec::new();
    collect_sanitizer_edits(source, &root.children, &definitions, &mut edits)
        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
    let mut rendered =
        apply_source_edits(source, edits).ok_or(ProposalPreparationFailure::RawParseFallback)?;
    let Node::Root(mut rendered_root) = to_mdast(&rendered, &ParseOptions::gfm())
        .map_err(|_| ProposalPreparationFailure::RawParseFallback)?
    else {
        return Err(ProposalPreparationFailure::RawParseFallback);
    };
    if contains_activatable_markdown(&rendered_root.children) {
        let definitions = collect_effective_definition_urls(&rendered_root.children);
        let mut edits = Vec::new();
        collect_sanitizer_edits(&rendered, &rendered_root.children, &definitions, &mut edits)
            .ok_or(ProposalPreparationFailure::RawParseFallback)?;
        rendered = apply_source_edits(&rendered, edits)
            .ok_or(ProposalPreparationFailure::RawParseFallback)?;
        let Node::Root(reparsed_root) = to_mdast(&rendered, &ParseOptions::gfm())
            .map_err(|_| ProposalPreparationFailure::RawParseFallback)?
        else {
            return Err(ProposalPreparationFailure::RawParseFallback);
        };
        rendered_root = reparsed_root;
    }
    if contains_activatable_markdown(&rendered_root.children) {
        return Err(ProposalPreparationFailure::RawParseFallback);
    }
    Ok(rendered)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TableFingerprint {
    alignment: Vec<markdown::mdast::AlignKind>,
    row_cell_counts: Vec<usize>,
}

fn table_fingerprint(table: &markdown::mdast::Table) -> Option<TableFingerprint> {
    let mut row_cell_counts = Vec::with_capacity(table.children.len());
    for row in &table.children {
        let Node::TableRow(row) = row else {
            return None;
        };
        if row
            .children
            .iter()
            .any(|cell| !matches!(cell, Node::TableCell(_)))
        {
            return None;
        }
        row_cell_counts.push(row.children.len());
    }
    Some(TableFingerprint {
        alignment: table.align.clone(),
        row_cell_counts,
    })
}

fn proposal_table_render_width_px(table: &markdown::mdast::Table) -> Option<u16> {
    let mut column_lengths = Vec::new();
    for row in &table.children {
        let Node::TableRow(row) = row else {
            return None;
        };
        for (column, cell) in row.children.iter().enumerate() {
            if !matches!(cell, Node::TableCell(_)) {
                return None;
            }
            if column_lengths.len() <= column {
                column_lengths.resize(column + 1, DEFAULT_TABLE_COLUMN_LENGTH);
            }
            let length = cell.to_string().len().min(MAX_TABLE_COLUMN_LENGTH);
            column_lengths[column] = column_lengths[column]
                .max(length)
                .min(MAX_TABLE_COLUMN_LENGTH);
        }
    }

    let total_length = column_lengths
        .iter()
        .try_fold(0usize, |total, length| total.checked_add(*length))?;
    let width_per_byte = usize::from(APP_TEXT_SIZE.ceil());
    let content_width = total_length.checked_mul(width_per_byte)?;
    let column_chrome = column_lengths.len().checked_mul(TABLE_COLUMN_CHROME_PX)?;
    content_width
        .checked_add(column_chrome)?
        .checked_add(TABLE_OUTER_BORDER_PX)
        .map(|width| width.clamp(MIN_TABLE_RENDER_WIDTH_PX, MAX_TABLE_RENDER_WIDTH_PX))
        .and_then(|width| u16::try_from(width).ok())
}

fn parsed_table_fingerprint(source: &str) -> Option<TableFingerprint> {
    let Node::Root(root) = to_mdast(source, &ParseOptions::gfm()).ok()? else {
        return None;
    };
    let [Node::Table(table)] = root.children.as_slice() else {
        return None;
    };
    table_fingerprint(table)
}

fn source_line_start_offset(source: &str, line: usize) -> Option<usize> {
    if line == 0 {
        return None;
    }
    if line == 1 {
        return Some(0);
    }
    let bytes = source.as_bytes();
    let mut current_line = 1;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\n' || (bytes[index] == b'\r' && bytes.get(index + 1) != Some(&b'\n'))
        {
            current_line += 1;
            if current_line == line {
                return Some(index + 1);
            }
        }
        index += 1;
    }
    None
}

fn validate_table_start_position(source: &str, position: &markdown::unist::Position) -> bool {
    if position.start.column == 0
        || position.start.offset > position.end.offset
        || source
            .get(position.start.offset..position.end.offset)
            .is_none()
    {
        return false;
    }
    let Some(line_start) = source_line_start_offset(source, position.start.line) else {
        return false;
    };
    let Some(prefix) = source.get(line_start..position.start.offset) else {
        return false;
    };
    let mut column = 1usize;
    for character in prefix.chars() {
        if character == '\t' {
            let remainder = column % 4;
            column = column.saturating_add(if remainder == 0 { 1 } else { 5 - remainder });
        } else {
            column = column.saturating_add(1);
        }
    }
    column == position.start.column
}

fn prefix_bytes_to_table_column(line: &str, target_column: usize) -> Option<usize> {
    let mut visual_column = 0usize;
    let mut byte_offset = 0usize;
    while visual_column < target_column {
        let byte = *line.as_bytes().get(byte_offset)?;
        match byte {
            b' ' => {
                visual_column = visual_column.checked_add(1)?;
                byte_offset += 1;
            }
            b'\t' => {
                let tab_width = 4 - (visual_column % 4);
                visual_column = visual_column.checked_add(tab_width)?;
                if visual_column > target_column {
                    return None;
                }
                byte_offset += 1;
            }
            b'>' => {
                visual_column = visual_column.checked_add(1)?;
                if visual_column > target_column {
                    return None;
                }
                byte_offset += 1;
            }
            _ => return None,
        }
    }
    (visual_column == target_column).then_some(byte_offset)
}

fn split_table_line(line: &str) -> (&str, &str) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else if let Some(body) = line.strip_suffix('\r') {
        (body, "\r")
    } else {
        (line, "")
    }
}

fn normalize_table_source(
    source: &str,
    position: &markdown::unist::Position,
    raw: &str,
) -> Option<String> {
    if !validate_table_start_position(source, position) {
        return None;
    }
    let target_column = position.start.column.checked_sub(1)?;
    let mut normalized = String::with_capacity(raw.len());
    let mut first_line = true;
    let mut cursor = 0;
    while cursor < raw.len() {
        let remaining = &raw[cursor..];
        let segment_len = remaining
            .find(['\r', '\n'])
            .map_or(remaining.len(), |index| {
                if remaining.as_bytes().get(index) == Some(&b'\r')
                    && remaining.as_bytes().get(index + 1) == Some(&b'\n')
                {
                    index + 2
                } else {
                    index + 1
                }
            });
        let segment = &remaining[..segment_len];
        let (body, ending) = split_table_line(segment);
        if first_line {
            normalized.push_str(segment);
            first_line = false;
        } else {
            let prefix_len = prefix_bytes_to_table_column(body, target_column)?;
            normalized.push_str(body.get(prefix_len..)?);
            normalized.push_str(ending);
        }
        cursor += segment_len;
    }
    (!first_line).then_some(normalized)
}

fn prepare_table_source(
    source: &str,
    table: &markdown::mdast::Table,
) -> Result<(String, TableFingerprint), ProposalPreparationFailure> {
    let fingerprint =
        table_fingerprint(table).ok_or(ProposalPreparationFailure::RawParseFallback)?;
    let position = table
        .position
        .as_ref()
        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
    if !validate_table_start_position(source, position) {
        return Err(ProposalPreparationFailure::RawParseFallback);
    }
    let raw = proposal_source_slice(source, Some(position))
        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
    if parsed_table_fingerprint(&raw).is_some_and(|parsed| parsed == fingerprint) {
        return Ok((raw, fingerprint));
    }
    let normalized = normalize_table_source(source, position, &raw)
        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
    if parsed_table_fingerprint(&normalized).is_some_and(|parsed| parsed == fingerprint) {
        Ok((normalized, fingerprint))
    } else {
        Err(ProposalPreparationFailure::RawParseFallback)
    }
}

fn validate_prepared_table_source(
    source: &str,
    fingerprint: &TableFingerprint,
    expected_definition_ids: &[&str],
) -> bool {
    let Ok(Node::Root(root)) = to_mdast(source, &ParseOptions::gfm()) else {
        return false;
    };
    let Some(Node::Table(table)) = root.children.first() else {
        return false;
    };
    if table_fingerprint(table).as_ref() != Some(fingerprint)
        || root.children.len() != expected_definition_ids.len() + 1
    {
        return false;
    }
    root.children[1..]
        .iter()
        .map(|node| match node {
            Node::Definition(definition) => Some(definition.identifier.as_str()),
            _ => None,
        })
        .eq(expected_definition_ids.iter().copied().map(Some))
}

#[cfg(test)]
fn parse_proposal_blocks(source: &str) -> Option<Vec<ProposalBlock>> {
    match prepare_proposal_presentation(source) {
        ProposalPresentation::Prepared(prepared) => Some(prepared.blocks.clone()),
        ProposalPresentation::RawParseFallback(_) | ProposalPresentation::TooComplex => None,
    }
}

fn prepare_proposal_presentation(source: &str) -> ProposalPresentation {
    let prepared = (|| {
        let Node::Root(root) = to_mdast(source, &ParseOptions::gfm())
            .map_err(|_| ProposalPreparationFailure::RawParseFallback)?
        else {
            return Err(ProposalPreparationFailure::RawParseFallback);
        };
        ensure_mdast_node_limit(&root.children)?;
        let definitions = collect_effective_definitions(source, &root.children)
            .ok_or(ProposalPreparationFailure::RawParseFallback)?;
        let mut budget = PreparationBudget::default();
        let mut table_ordinal = 0;
        let blocks = parse_proposal_block_nodes(
            source,
            &root.children,
            &definitions,
            &mut budget,
            &mut table_ordinal,
        )?;
        Ok(PreparedProposal {
            blocks,
            table_count: table_ordinal,
        })
    })();
    match prepared {
        Ok(prepared) => ProposalPresentation::Prepared(Arc::new(prepared)),
        Err(ProposalPreparationFailure::RawParseFallback) => {
            ProposalPresentation::RawParseFallback(inert_raw_fallback_source(source))
        }
        Err(ProposalPreparationFailure::TooComplex) => ProposalPresentation::TooComplex,
    }
}

fn ensure_mdast_node_limit(root_children: &[Node]) -> Result<(), ProposalPreparationFailure> {
    let mut nodes = root_children.iter().collect::<Vec<_>>();
    let mut count = 1usize;
    if count > MAX_MDAST_NODES {
        return Err(ProposalPreparationFailure::TooComplex);
    }
    while let Some(node) = nodes.pop() {
        count = count
            .checked_add(1)
            .ok_or(ProposalPreparationFailure::TooComplex)?;
        if count > MAX_MDAST_NODES {
            return Err(ProposalPreparationFailure::TooComplex);
        }
        if let Some(children) = node.children() {
            nodes.extend(children);
        }
    }
    Ok(())
}

#[derive(Default)]
struct PreparationBudget {
    weighted_complexity: usize,
    prepared_source_bytes: usize,
}

impl PreparationBudget {
    fn account_weight(&mut self, weight: usize) -> Result<(), ProposalPreparationFailure> {
        let next = self
            .weighted_complexity
            .checked_add(weight)
            .ok_or(ProposalPreparationFailure::TooComplex)?;
        if next > MAX_RENDER_COMPLEXITY {
            return Err(ProposalPreparationFailure::TooComplex);
        }
        self.weighted_complexity = next;
        Ok(())
    }

    fn account_markdown(&mut self, source_bytes: usize) -> Result<(), ProposalPreparationFailure> {
        let next_source_bytes = self
            .prepared_source_bytes
            .checked_add(source_bytes)
            .ok_or(ProposalPreparationFailure::TooComplex)?;
        if next_source_bytes > MAX_PREPARED_SOURCE_BYTES {
            return Err(ProposalPreparationFailure::TooComplex);
        }
        self.account_weight(3)?;
        self.prepared_source_bytes = next_source_bytes;
        Ok(())
    }

    fn account_list_item(&mut self) -> Result<(), ProposalPreparationFailure> {
        self.account_weight(1)
    }

    fn account_blockquote(&mut self) -> Result<(), ProposalPreparationFailure> {
        self.account_weight(1)
    }
}

fn referenced_definitions<'a>(
    references: &BTreeSet<String>,
    definitions: &'a [(String, String)],
) -> Vec<&'a str> {
    definitions
        .iter()
        .filter(|(identifier, _)| references.contains(identifier))
        .map(|(_, source)| source.as_str())
        .collect()
}

fn prepared_markdown_source_len(
    source_len: usize,
    referenced: &[&str],
) -> Result<usize, ProposalPreparationFailure> {
    if referenced.is_empty() {
        return Ok(source_len);
    }
    let appendix_len = referenced.iter().try_fold(0usize, |length, source| {
        length
            .checked_add(source.len())
            .ok_or(ProposalPreparationFailure::TooComplex)
    })?;
    let separators = referenced
        .len()
        .checked_sub(1)
        .ok_or(ProposalPreparationFailure::TooComplex)?;
    source_len
        .checked_add(2)
        .and_then(|length| length.checked_add(appendix_len))
        .and_then(|length| length.checked_add(separators))
        .ok_or(ProposalPreparationFailure::TooComplex)
}

fn collect_effective_definitions(source: &str, nodes: &[Node]) -> Option<Vec<(String, String)>> {
    let mut definitions = Vec::new();
    let mut seen = BTreeSet::new();
    collect_definition_nodes(source, nodes, &mut seen, &mut definitions)?;
    Some(definitions)
}

fn collect_effective_definition_urls(nodes: &[Node]) -> BTreeMap<String, String> {
    let mut definitions = BTreeMap::new();
    let mut pending = nodes.iter().rev().collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        if let Node::Definition(definition) = node {
            definitions
                .entry(definition.identifier.clone())
                .or_insert_with(|| definition.url.clone());
        }
        if let Some(children) = node.children() {
            pending.extend(children.iter().rev());
        }
    }
    definitions
}

fn collect_definition_nodes(
    source: &str,
    nodes: &[Node],
    seen: &mut BTreeSet<String>,
    definitions: &mut Vec<(String, String)>,
) -> Option<()> {
    let mut pending = nodes.iter().rev().collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        if let Node::Definition(definition) = node {
            let source_slice = proposal_source_slice(source, definition.position.as_ref())?;
            if seen.insert(definition.identifier.clone()) {
                definitions.push((definition.identifier.clone(), source_slice));
            }
        }
        if let Some(children) = node.children() {
            pending.extend(children.iter().rev());
        }
    }
    Some(())
}

fn parse_proposal_block_nodes(
    source: &str,
    nodes: &[Node],
    definitions: &[(String, String)],
    budget: &mut PreparationBudget,
    table_ordinal: &mut usize,
) -> Result<Vec<ProposalBlock>, ProposalPreparationFailure> {
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < nodes.len() {
        let block = match &nodes[index] {
            Node::Definition(definition) => {
                proposal_source_slice(source, definition.position.as_ref())
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                index += 1;
                continue;
            }
            Node::List(list) => {
                // Validate the list range even though its children provide the rendered text.
                proposal_source_slice(source, list.position.as_ref())
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                let items = list
                    .children
                    .iter()
                    .map(|child| {
                        let Node::ListItem(item) = child else {
                            return Err(ProposalPreparationFailure::RawParseFallback);
                        };
                        proposal_source_slice(source, item.position.as_ref())
                            .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                        budget.account_list_item()?;
                        Ok(ProposalListItem {
                            checked: item.checked,
                            blocks: parse_proposal_block_nodes(
                                source,
                                &item.children,
                                definitions,
                                budget,
                                table_ordinal,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                ProposalBlock::List(ProposalList {
                    ordered: list.ordered,
                    start: list.start,
                    items,
                })
            }
            Node::Blockquote(blockquote) => {
                proposal_source_slice(source, blockquote.position.as_ref())
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                budget.account_blockquote()?;
                ProposalBlock::Blockquote(ProposalBlockquote {
                    blocks: parse_proposal_block_nodes(
                        source,
                        &blockquote.children,
                        definitions,
                        budget,
                        table_ordinal,
                    )?,
                })
            }
            Node::Table(table) => {
                let (source_slice, fingerprint) = prepare_table_source(source, table)?;
                let render_width_px = proposal_table_render_width_px(table)
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                let mut references = BTreeSet::new();
                collect_reference_ids(&Node::Table(table.clone()), &mut references);
                let referenced = referenced_definitions(&references, definitions);
                let expected_definition_ids = definitions
                    .iter()
                    .filter(|(identifier, _)| references.contains(identifier))
                    .map(|(identifier, _)| identifier.as_str())
                    .collect::<Vec<_>>();
                let _ = prepared_markdown_source_len(source_slice.len(), &referenced)?;
                let source = if referenced.is_empty() {
                    source_slice
                } else {
                    format!("{source_slice}\n\n{}", referenced.join("\n"))
                };
                if !validate_prepared_table_source(&source, &fingerprint, &expected_definition_ids)
                {
                    return Err(ProposalPreparationFailure::RawParseFallback);
                }
                let render_source = sanitize_render_source(&source)?;
                if !validate_prepared_table_source(
                    &render_source,
                    &fingerprint,
                    &expected_definition_ids,
                ) {
                    return Err(ProposalPreparationFailure::RawParseFallback);
                }
                budget.account_markdown(render_source.len())?;
                let ordinal = *table_ordinal;
                *table_ordinal = (*table_ordinal)
                    .checked_add(1)
                    .ok_or(ProposalPreparationFailure::TooComplex)?;
                ProposalBlock::Table(ProposalTable {
                    source,
                    render_source,
                    ordinal,
                    render_width_px,
                })
            }
            _ => {
                let start = nodes[index]
                    .position()
                    .and_then(|position| {
                        source
                            .get(position.start.offset..position.start.offset)
                            .map(|_| position.start.offset)
                    })
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                let mut end = start;
                let mut references = BTreeSet::new();
                let mut next = index;
                while next < nodes.len()
                    && !matches!(
                        nodes[next],
                        Node::Definition(_) | Node::Table(_) | Node::List(_) | Node::Blockquote(_)
                    )
                {
                    let position = nodes[next]
                        .position()
                        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                    end = position.end.offset;
                    collect_reference_ids(&nodes[next], &mut references);
                    next += 1;
                }
                let source_slice = source
                    .get(start..end)
                    .map(str::to_owned)
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                let referenced = referenced_definitions(&references, definitions);
                let _ = prepared_markdown_source_len(source_slice.len(), &referenced)?;
                let source_slice = if referenced.is_empty() {
                    source_slice
                } else {
                    format!("{source_slice}\n\n{}", referenced.join("\n"))
                };
                let source_slice = sanitize_render_source(&source_slice)?;
                budget.account_markdown(source_slice.len())?;
                index = next;
                ProposalBlock::Markdown(source_slice)
            }
        };
        if matches!(
            &block,
            ProposalBlock::Table(_) | ProposalBlock::List(_) | ProposalBlock::Blockquote(_)
        ) {
            index += 1;
        }
        blocks.push(block);
    }
    Ok(blocks)
}

fn collect_reference_ids(node: &Node, references: &mut BTreeSet<String>) {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node {
            Node::LinkReference(reference) => {
                references.insert(reference.identifier.clone());
            }
            Node::ImageReference(reference) => {
                references.insert(reference.identifier.clone());
            }
            _ => {}
        }
        if let Some(children) = node.children() {
            pending.extend(children);
        }
    }
}

fn render_proposal_blocks(
    blocks: &[ProposalBlock],
    description_id: &SharedString,
    path: &str,
    identity: &ProposalIdentity,
    table_scroll_handles: &BTreeMap<String, ScrollHandle>,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) -> gpui::AnyElement {
    let mut container = div().w_full().min_w(px(0.0)).flex().flex_col().gap_2();
    for (index, block) in blocks.iter().enumerate() {
        let block_path = format!("{path}-{index}");
        let element = match block {
            ProposalBlock::Markdown(source) => TextView::markdown(
                SharedString::from(format!("{description_id}-text-{block_path}")),
                source.clone(),
                window,
                cx,
            )
            .selectable(true)
            .into_any_element(),
            ProposalBlock::Table(table) => {
                let scroll = table_scroll_handles
                    .get(&proposal_table_scroll_key(identity, table.ordinal))
                    .cloned()
                    .unwrap_or_else(ScrollHandle::new);
                let table_id = format!(
                    "{description_id}-table-{}-{}",
                    identity.contract_address, table.ordinal
                );
                let text = TextView::markdown(
                    SharedString::from(format!("{table_id}-text")),
                    table.render_source.clone(),
                    window,
                    cx,
                )
                .w(px(f32::from(table.render_width_px)))
                .min_w_full()
                .flex_none()
                .selectable(true);
                let mut table_scroller = div()
                    .id(SharedString::from(format!("{table_id}-scroll")))
                    .w_full()
                    .min_w(px(0.0))
                    .track_scroll(&scroll)
                    .overflow_x_scroll()
                    .horizontal_scrollbar(&scroll)
                    .relative();
                table_scroller.style().restrict_scroll_to_axis = Some(true);
                div()
                    .id(SharedString::from(format!("{table_id}-wrapper")))
                    .w_full()
                    .min_w(px(0.0))
                    .child(table_scroller.child(text))
                    .into_any_element()
            }
            ProposalBlock::List(list) => render_proposal_list(
                list,
                description_id,
                &block_path,
                identity,
                table_scroll_handles,
                window,
                cx,
            ),
            ProposalBlock::Blockquote(blockquote) => div()
                .id(SharedString::from(format!(
                    "{description_id}-blockquote-{block_path}"
                )))
                .w_full()
                .min_w(px(0.0))
                .pl(px(12.0))
                .border_l_2()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .child(render_proposal_blocks(
                    &blockquote.blocks,
                    description_id,
                    &block_path,
                    identity,
                    table_scroll_handles,
                    window,
                    cx,
                ))
                .into_any_element(),
        };
        container = container.child(element);
    }
    container.into_any_element()
}

fn render_proposal_list(
    list: &ProposalList,
    description_id: &SharedString,
    path: &str,
    identity: &ProposalIdentity,
    table_scroll_handles: &BTreeMap<String, ScrollHandle>,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) -> gpui::AnyElement {
    let mut container = div().w_full().min_w(px(0.0)).flex().flex_col().gap_2();
    let start = list.start.unwrap_or(1);
    for (index, item) in list.items.iter().enumerate() {
        let marker = if let Some(checked) = item.checked {
            if checked { "[x]" } else { "[ ]" }.to_owned()
        } else if list.ordered {
            format!("{}.", start.saturating_add(index as u32))
        } else {
            "•".to_owned()
        };
        let item_path = format!("{path}-{index}");
        let body = render_proposal_blocks(
            &item.blocks,
            description_id,
            &item_path,
            identity,
            table_scroll_handles,
            window,
            cx,
        );
        container = container.child(
            div()
                .id(SharedString::from(format!(
                    "{description_id}-list-row-{item_path}"
                )))
                .w_full()
                .min_w(px(0.0))
                .flex()
                .items_start()
                .child(
                    div()
                        .w(px(32.0))
                        .flex_none()
                        .child(app_muted_text(marker).text_size(px(14.0))),
                )
                .child(div().flex_1().min_w(px(0.0)).child(body)),
        );
    }
    container.into_any_element()
}

fn render_proposal_detail_tabs(
    root: &Entity<WalletRoot>,
    selected: ProposalDetailTab,
    action_count: usize,
    identity: &ProposalIdentity,
) -> impl IntoElement {
    let selected_index = match selected {
        ProposalDetailTab::Description => 0,
        ProposalDetailTab::Actions => 1,
    };
    let tab_root = root.clone();
    TabBar::new(SharedString::from(format!(
        "wallet-proposal-detail-tabs-{}-{}-{}",
        proposal_version_label(identity.contract_version),
        identity.contract_address,
        identity.index,
    )))
    .underline()
    .w_full()
    .flex_none()
    .selected_index(selected_index)
    .on_click(move |index, _window, cx| {
        let tab = match *index {
            0 => ProposalDetailTab::Description,
            1 => ProposalDetailTab::Actions,
            _ => return,
        };
        tab_root.update(cx, |root, cx| root.select_proposal_detail_tab(tab, cx));
    })
    .children([
        Tab::new().min_w(px(112.0)).label("Description"),
        Tab::new()
            .min_w(px(112.0))
            .label(format!("Actions ({action_count})")),
    ])
}

fn action_calldata_hex(calldata: &alloy::primitives::Bytes) -> String {
    format!("0x{}", alloy::hex::encode(calldata))
}

fn compact_calldata_display(value: &str) -> String {
    if value.chars().count() <= 42 {
        return value.to_owned();
    }
    let prefix = value.chars().take(20).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(16)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

fn proposal_action_id(
    identity: &ProposalIdentity,
    ordinal: usize,
    field: &str,
    control: &str,
) -> SharedString {
    SharedString::from(format!(
        "wallet-proposal-action-{}-{}-{}-{ordinal}-{field}-{control}",
        proposal_version_label(identity.contract_version),
        identity.contract_address,
        identity.index,
    ))
}

fn render_proposal_actions_card(
    root: &Entity<WalletRoot>,
    proposal: &ResolvedProposal,
    chain_id: u64,
    expanded_calldata: &BTreeSet<ProposalActionIdentity>,
) -> impl IntoElement {
    let identity = proposal.identity();
    let mut card = div()
        .id(SharedString::from(format!(
            "wallet-proposal-actions-{}-{}-{}",
            proposal_version_label(identity.contract_version),
            identity.contract_address,
            identity.index,
        )))
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_3()
        .p(px(20.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER));
    if proposal.proposal.actions.is_empty() {
        return card.child(app_muted_text("No contract actions"));
    }
    for (ordinal, action) in proposal.proposal.actions.iter().enumerate() {
        let action_identity = ProposalActionIdentity {
            proposal: identity.clone(),
            ordinal,
        };
        let calldata = action_calldata_hex(&action.calldata);
        let expanded = expanded_calldata.contains(&action_identity);
        let compact_calldata = compact_calldata_display(&calldata);
        let address = action.call_contract.to_checksum(None);
        let value = action.value;
        let address_copy_id = proposal_action_id(&identity, ordinal, "address", "clipboard");
        let calldata_copy_id = proposal_action_id(&identity, ordinal, "calldata", "clipboard");
        let calldata_toggle_id = proposal_action_id(&identity, ordinal, "calldata", "toggle");
        let calldata_copy_wrapper_id =
            proposal_action_id(&identity, ordinal, "calldata", "copy-wrapper");
        let calldata_toggle_root = root.clone();
        let calldata_toggle_identity = identity.clone();
        let mut action_card = div()
            .id(proposal_action_id(&identity, ordinal, "wrapper", "card"))
            .w_full()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .gap_2()
            .p(px(12.0))
            .rounded_md()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .border_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .child(app_strong_text(format!("Action {}", ordinal + 1)).text_size(px(13.0)))
            .child(app_muted_text("Target").text_size(px(11.0)))
            .child(
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_text(address.clone())
                            .flex_none()
                            .font_family(APP_MONO_FONT_FAMILY)
                            .text_size(px(12.0))
                            .whitespace_nowrap(),
                    )
                    .child(
                        div()
                            .id(proposal_action_id(&identity, ordinal, "address", "action"))
                            .flex_none()
                            .tooltip(|window, cx| Tooltip::new("Copy target").build(window, cx))
                            .child(clipboard_with_toast(address_copy_id, address.clone())),
                    ),
            )
            .child(app_muted_text("Calldata").text_size(px(11.0)))
            .child(
                Collapsible::new()
                    .open(expanded)
                    .w_full()
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                app_button_base(calldata_toggle_id)
                                    .text()
                                    .small()
                                    .compact()
                                    .icon(if expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(
                                        app_text(compact_calldata)
                                            .font_family(APP_MONO_FONT_FAMILY)
                                            .text_size(px(12.0))
                                            .whitespace_nowrap(),
                                    )
                                    .on_click(move |_event, _window, cx| {
                                        cx.stop_propagation();
                                        calldata_toggle_root.update(cx, |root, cx| {
                                            root.toggle_proposal_calldata(
                                                calldata_toggle_identity.clone(),
                                                ordinal,
                                                cx,
                                            );
                                        });
                                    }),
                            ),
                    )
                    .content(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .pl(px(14.0))
                            .pt(px(8.0))
                            .pb(px(7.0))
                            .child(
                                div()
                                    .id(proposal_action_id(
                                        &identity, ordinal, "calldata", "expanded",
                                    ))
                                    .w_full()
                                    .min_w(px(0.0))
                                    .relative()
                                    .child(
                                        div()
                                            .absolute()
                                            .top(px(-2.0))
                                            .right(px(0.0))
                                            .id(calldata_copy_wrapper_id)
                                            .tooltip(|window, cx| {
                                                Tooltip::new("Copy calldata").build(window, cx)
                                            })
                                            .child(clipboard_with_toast(
                                                calldata_copy_id,
                                                calldata.clone(),
                                            )),
                                    )
                                    .child(
                                        div()
                                            .w_full()
                                            .min_w(px(0.0))
                                            .pr(px(28.0))
                                            .font_family(APP_MONO_FONT_FAMILY)
                                            .text_size(px(12.0))
                                            .line_height(px(16.0))
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .whitespace_normal()
                                            .child(SharedString::from(calldata)),
                                    ),
                            ),
                    ),
            );
        if !value.is_zero() {
            action_card = action_card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(app_muted_text("Value").text_size(px(11.0)))
                    .child(
                        app_text(format_native_token_amount_for_display(chain_id, value))
                            .text_size(px(12.0))
                            .font_family(APP_MONO_FONT_FAMILY),
                    ),
            );
        }
        card = card.child(action_card);
    }
    card
}

fn proposal_document_card(
    proposal: &ResolvedProposal,
    description_id: SharedString,
    table_scroll_handles: &BTreeMap<String, ScrollHandle>,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) -> gpui::Div {
    let mut card = div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_2()
        .p(px(20.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER));
    let mut header = div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(app_strong_text("Proposal").text_size(px(13.0)))
        .child(div().flex_1());
    if let Some(document) = proposal.document().filter(|document| document.available) {
        header = header.child(
            div()
                .id(proposal_copy_id(proposal, "proposal", "action"))
                .tooltip(|window, cx| Tooltip::new("Copy proposal text").build(window, cx))
                .child(clipboard_with_toast(
                    proposal_copy_id(proposal, "proposal", "clipboard"),
                    document.description.clone(),
                )),
        );
    }
    let content = match proposal.document() {
        None => div()
            .flex()
            .flex_col()
            .gap_2()
            .children([520.0_f32, 460.0, 500.0, 340.0].into_iter().map(|width| {
                Skeleton::new()
                    .secondary()
                    .h(px(14.0))
                    .w(px(width))
                    .max_w_full()
            }))
            .into_any_element(),
        Some(document) if !document.available => div()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .p(px(24.0))
            .child(
                Icon::new(IconName::TriangleAlert)
                    .size_6()
                    .text_color(rgb(theme::TEXT_MUTED)),
            )
            .child(app_muted_text(
                "Document could not be retrieved from IPFS gateways.",
            ))
            .child(app_muted_text("Refresh retries the download.").text_size(px(12.0)))
            .into_any_element(),
        Some(document) => match proposal.presentation() {
            Some(ProposalPresentation::Prepared(prepared)) => render_proposal_blocks(
                &prepared.blocks,
                &description_id,
                "root",
                &proposal.identity(),
                table_scroll_handles,
                window,
                cx,
            ),
            Some(ProposalPresentation::RawParseFallback(source)) => {
                TextView::markdown(description_id, source.clone(), window, cx)
                    .selectable(true)
                    .into_any_element()
            }
            None => TextView::markdown(
                description_id,
                inert_raw_fallback_source(&document.description),
                window,
                cx,
            )
            .selectable(true)
            .into_any_element(),
            Some(ProposalPresentation::TooComplex) => div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .p(px(24.0))
                .child(app_muted_text(
                    "This proposal is too complex to render safely.",
                ))
                .child(
                    app_muted_text("Copy the proposal text to view it externally.")
                        .text_size(px(12.0)),
                )
                .into_any_element(),
        },
    };
    card = card.child(header).child(content);
    card
}
fn proposal_card(title: &'static str) -> gpui::Div {
    div()
        .w_full()
        .p(px(14.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .flex()
        .flex_col()
        .gap_2()
        .child(app_strong_text(title).text_size(px(13.0)))
}
fn sponsorship_per_mille(proposal: &ResolvedProposal) -> u16 {
    let scale = U256::from(10).pow(U256::from(18));
    let threshold = u128::try_from(proposal.rules.sponsor_threshold / scale).unwrap_or(0);
    let sponsored = u128::try_from(proposal.proposal.sponsorship / scale).unwrap_or(0);
    per_mille(U256::from(sponsored), U256::from(threshold))
}
fn render_sponsorship_card(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let per_mille = sponsorship_per_mille(proposal);
    let fraction = ratio_fraction(per_mille);
    let color = if status.stage == GovernanceProposalStage::SponsorshipExpired {
        theme::TEXT_MUTED
    } else {
        theme::PRIMARY
    };
    let percent = (u32::from(per_mille) + 5) / 10;
    let mut footer_row = div()
        .flex()
        .justify_between()
        .child(app_muted_text(format!("{percent}% of threshold")).text_size(px(11.0)));
    if status.stage == GovernanceProposalStage::SponsorshipExpired {
        footer_row = footer_row.child(
            app_muted_text(format!(
                "Expired {}",
                format_deadline(&status.deadlines.sponsorship,)
            ))
            .text_size(px(11.0)),
        );
    } else if let Some(remaining) = countdown(&status.deadlines.sponsorship, chain_time) {
        footer_row =
            footer_row.child(app_muted_text(format!("Ends in {remaining}")).text_size(px(11.0)));
    }
    proposal_card("Sponsorship")
        .child(
            app_strong_text(format!(
                "{} / {} RAIL",
                format_compact_rail_amount(proposal.proposal.sponsorship),
                format_compact_rail_amount(proposal.rules.sponsor_threshold)
            ))
            .text_size(px(15.0)),
        )
        .child(
            div()
                .w_full()
                .h(px(6.0))
                .rounded_full()
                .overflow_hidden()
                .bg(rgb(theme::SURFACE_HOVER))
                .child(
                    div()
                        .w(px((300.0 - 28.0) * fraction))
                        .h(px(6.0))
                        .rounded_full()
                        .bg(rgb(color)),
                ),
        )
        .child(footer_row)
}
fn render_votes_card(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let split = vote_split(proposal.proposal.yay_votes, proposal.proposal.nay_votes);
    let for_fraction = split.map_or(0.0, ratio_fraction);
    let gap = if split.is_some_and(|value| value > 0 && value < 1_000) {
        2.0
    } else {
        0.0
    };
    let width = 272.0;
    let meter = div()
        .w_full()
        .h(px(6.0))
        .flex()
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(theme::SURFACE_HOVER))
        .children(split.map(|_| {
            div()
                .w(px((width - gap) * for_fraction))
                .h(px(6.0))
                .bg(rgb(theme::PRIMARY))
        }))
        .children(
            split
                .filter(|_| gap > 0.0)
                .map(|_| div().w(px(gap)).h(px(6.0)).bg(rgb(theme::SURFACE))),
        )
        .children(split.map(|_| div().flex_1().h(px(6.0)).bg(rgb(theme::DANGER))));
    let legend = |label: &'static str, amount: U256, color: u32, fraction: Option<u16>| {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().size(px(8.0)).rounded_full().bg(rgb(color)))
            .child(app_muted_text(label).flex_1())
            .child(
                app_text(format_compact_rail_amount_with_unit(amount))
                    .text_color(rgb(theme::TEXT))
                    .text_size(px(12.0)),
            )
            .children(
                fraction
                    .map(|value| app_muted_text(format_ratio_percent(value)).text_size(px(11.0))),
            )
    };
    proposal_card("Votes")
        .child(meter)
        .children([
            legend("For", proposal.proposal.yay_votes, theme::PRIMARY, split),
            legend(
                "Against",
                proposal.proposal.nay_votes,
                theme::DANGER,
                split.map(|value| 1_000 - value),
            ),
        ])
        .children(
            split
                .is_none()
                .then(|| app_muted_text("No votes").text_size(px(11.0))),
        )
        .child(
            app_muted_text(format!(
                "Sponsored {} / {} RAIL",
                format_compact_rail_amount(proposal.proposal.sponsorship),
                format_compact_rail_amount(proposal.rules.sponsor_threshold)
            ))
            .text_size(px(11.0)),
        )
        .child(
            app_muted_text(format!(
                "Quorum: {} / {} · {}",
                format_compact_rail_amount(status.quorum_progress),
                format_compact_rail_amount(status.quorum),
                if status.quorum_met { "met" } else { "not met" },
            ))
            .text_size(px(11.0)),
        )
        .child(
            app_muted_text(match status.majority {
                wallet_ops::GovernanceMajorityResult::Yay => "Majority: yay",
                wallet_ops::GovernanceMajorityResult::Nay => "Majority: nay",
                wallet_ops::GovernanceMajorityResult::Tie => "Majority: tie",
            })
            .text_size(px(11.0)),
        )
        .children((status.stage == GovernanceProposalStage::Failed).then(|| {
            app_muted_text(if status.quorum_met {
                "Failed: yay majority not met"
            } else {
                "Failed: quorum not met"
            })
            .text_size(px(11.0))
        }))
}
fn timeline_step(
    icon: Option<IconName>,
    label: &'static str,
    value: String,
    color: u32,
    has_connector: bool,
) -> gpui::Div {
    let marker = icon.map_or_else(
        || {
            div()
                .size(px(10.0))
                .rounded_full()
                .border_1()
                .border_color(rgb(theme::BORDER))
        },
        |icon| {
            div()
                .size(px(14.0))
                .child(Icon::new(icon).text_color(rgb(color)))
        },
    );
    let mut body = div()
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_strong_text(label).text_size(px(12.0)))
        .child(app_muted_text(value).text_size(px(11.0)));
    if has_connector {
        body = body.mb(px(12.0));
    }
    div()
        .flex()
        .gap_2()
        .child(
            div()
                .w(px(14.0))
                .flex()
                .flex_col()
                .items_center()
                .child(marker)
                .children(has_connector.then(|| {
                    div()
                        .w(px(1.0))
                        .flex_1()
                        .min_h(px(12.0))
                        .bg(rgb(theme::BORDER_SUBTLE))
                })),
        )
        .child(body)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimelineDeadline {
    SponsorshipClose,
    VotingOpen,
    YayVotingEnd,
    NayVotingEnd,
    ExecutionOpen,
    ExecutionClose,
}

fn timeline_deadline_completed(
    milestone: TimelineDeadline,
    chain_time: U256,
    deadline: U256,
) -> bool {
    match milestone {
        TimelineDeadline::VotingOpen | TimelineDeadline::ExecutionOpen => chain_time > deadline,
        TimelineDeadline::SponsorshipClose
        | TimelineDeadline::YayVotingEnd
        | TimelineDeadline::NayVotingEnd
        | TimelineDeadline::ExecutionClose => chain_time >= deadline,
    }
}

fn render_timeline_card(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let sponsorship = if status.stage == GovernanceProposalStage::SponsorshipExpired {
        (
            Some(IconName::CircleX),
            format!("Expired {}", format_deadline(&status.deadlines.sponsorship)),
            theme::TEXT_MUTED,
        )
    } else if timeline_deadline_completed(
        TimelineDeadline::SponsorshipClose,
        chain_time,
        status.deadlines.sponsorship,
    ) {
        (
            Some(IconName::CircleCheck),
            format_deadline(&status.deadlines.sponsorship),
            theme::SUCCESS,
        )
    } else {
        (
            None,
            format_deadline(&status.deadlines.sponsorship),
            theme::TEXT_MUTED,
        )
    };
    let called = !proposal.proposal.vote_call_time.is_zero();
    let mut steps = div().flex().flex_col();
    steps = steps.child(timeline_step(
        Some(IconName::CircleCheck),
        "Published",
        format_datetime_short(&proposal.proposal.publish_time),
        theme::SUCCESS,
        true,
    ));
    steps = steps.child(timeline_step(
        sponsorship.0,
        "Sponsorship closes",
        sponsorship.1,
        sponsorship.2,
        true,
    ));
    steps = steps.child(timeline_step(
        called.then_some(IconName::CircleCheck),
        "Voting called",
        if called {
            format_datetime_short(&proposal.proposal.vote_call_time)
        } else {
            "Not called".to_string()
        },
        if called {
            theme::SUCCESS
        } else {
            theme::TEXT_MUTED
        },
        true,
    ));
    if called {
        for (milestone, label, deadline) in [
            (
                TimelineDeadline::VotingOpen,
                "Voting opens",
                status.deadlines.voting_start,
            ),
            (
                TimelineDeadline::YayVotingEnd,
                "Yay voting ends",
                status.deadlines.yay_end,
            ),
            (
                TimelineDeadline::NayVotingEnd,
                "Nay voting ends",
                status.deadlines.nay_end,
            ),
            (
                TimelineDeadline::ExecutionOpen,
                "Execution opens",
                status.deadlines.execution_start,
            ),
            (
                TimelineDeadline::ExecutionClose,
                "Execution closes",
                status.deadlines.execution_end,
            ),
        ] {
            if let Some(deadline) = deadline {
                let completed = timeline_deadline_completed(milestone, chain_time, deadline);
                steps = steps.child(timeline_step(
                    completed.then_some(IconName::CircleCheck),
                    label,
                    format_deadline(&deadline),
                    if completed {
                        theme::SUCCESS
                    } else {
                        theme::TEXT_MUTED
                    },
                    true,
                ));
            }
        }
    }
    if proposal.proposal.executed {
        steps = steps.child(timeline_step(
            Some(IconName::CircleCheck),
            "Executed",
            "Confirmed on chain".to_string(),
            theme::SUCCESS,
            false,
        ));
    }
    proposal_card("Timeline").child(steps)
}
fn render_details_card(proposal: &ResolvedProposal) -> gpui::Div {
    let proposer = proposal.proposal.proposer.to_checksum(None);
    let contract = proposal.proposal.contract_address.to_checksum(None);
    proposal_card("Details")
        .child(proposal_copy_control(
            proposal, "Proposer", proposer, "proposer",
        ))
        .child(proposal_copy_control(
            proposal,
            "IPFS CID",
            proposal.proposal.proposal_document.clone(),
            "document",
        ))
        .child(proposal_copy_control(
            proposal, "Contract", contract, "contract",
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(app_muted_text("Version").text_size(px(11.0)))
                .child(
                    app_text(match proposal.proposal.contract_version {
                        wallet_ops::GovernanceContractVersion::V2 => "V2 Voting",
                        wallet_ops::GovernanceContractVersion::V1 => "V1 VotingLegacy",
                    })
                    .text_size(px(12.0)),
                ),
        )
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Instant;

    use alloy::primitives::{Address, U256};
    use markdown::{ParseOptions, mdast::Node, to_mdast};

    use super::{
        CONTENT_WIDTH, DocumentCompletion, MAX_MDAST_NODES, MAX_PREPARED_SOURCE_BYTES,
        MAX_TABLE_RENDER_WIDTH_PX, PreparationBudget, ProposalActionIdentity, ProposalBlock,
        ProposalDetailTab, ProposalDocumentState, ProposalPreparationFailure, ProposalPresentation,
        ProposalSelection, ProposalsPage, ProposalsState, ResolvedProposal, TABLE_COLUMN_CHROME_PX,
        TABLE_OUTER_BORDER_PX, TimelineDeadline, action_calldata_hex, compact_calldata_display,
        ensure_mdast_node_limit, format_compact_rail_amount, format_compact_rail_amount_with_unit,
        inert_raw_fallback_source, list_voting_deadline, next_proposal_page, parse_proposal_blocks,
        prepare_proposal_presentation, prepared_markdown_source_len, proposal_action_id,
        proposal_table_scroll_key, send_document_completions, table_fingerprint,
        timeline_deadline_completed, vote_split,
    };
    use ui::theme::APP_TEXT_SIZE;
    use wallet_ops::{
        GovernanceContractRules, GovernanceContractSummary, GovernanceContractVersion,
        GovernanceDocument, GovernanceOverview, GovernanceProposal, GovernanceProposalAction,
        GovernanceProposalDeadlines, GovernanceProposalStage, GovernanceProposalStatus,
    };

    fn test_proposal(index: u64, document: ProposalDocumentState) -> ResolvedProposal {
        ResolvedProposal {
            proposal: GovernanceProposal {
                contract_version: wallet_ops::GovernanceContractVersion::V2,
                index: U256::from(index),
                contract_address: Address::ZERO,
                proposer: Address::ZERO,
                proposal_document: String::new(),
                publish_time: U256::ZERO,
                vote_call_time: U256::ZERO,
                sponsorship: U256::ZERO,
                executed: false,
                yay_votes: U256::ZERO,
                nay_votes: U256::ZERO,
                sponsor_snapshot_interval: U256::ZERO,
                voting_snapshot_interval: U256::ZERO,
                actions: Vec::new(),
            },
            rules: GovernanceContractRules {
                sponsor_threshold: U256::ZERO,
                quorum: U256::ZERO,
                sponsor_window: U256::from(1),
                voting_start_offset: U256::from(1),
                voting_yay_end_offset: U256::from(2),
                voting_nay_end_offset: U256::from(3),
                execution_start_offset: U256::from(4),
                execution_end_offset: U256::from(5),
            },
            document,
        }
    }

    fn resolved_document() -> ProposalDocumentState {
        resolved_document_with_title("resolved")
    }

    fn resolved_document_with_title(title: &str) -> ProposalDocumentState {
        ProposalDocumentState::Resolved {
            document: GovernanceDocument {
                title: title.to_string(),
                description: String::new(),
                available: true,
            },
            presentation: ProposalPresentation::RawParseFallback(inert_raw_fallback_source("")),
        }
    }

    #[test]
    fn proposal_block_parser_preserves_all_description_list_items() {
        let source = "Making way for Railgun v3, this proposal:\n\n- Aims to make it easier for others (through a logic contract upgrade) to build & run public broadcaster code, ideally resulting in a more reliable and easier to use broadcaster network.\n- Updates the cyptographic circuits of RAILGUN to improve gas efficiency, remove redundant code for efficiency purposes, and add additional security features.\n- Adds new randomness to the circuits via an additional ceremony.\n- No other changes.\n\nThis will be a full upgrade of the RAILGUN logic contract and circuits if the YES vote is successful.\n\nPlease feel free to read through the code of this RAILGUN v2.5 proposal and vote YES if you support these upgrades.\n";
        let blocks = parse_proposal_blocks(source).expect("description should parse");
        assert_eq!(blocks.len(), 3);
        let ProposalBlock::List(list) = &blocks[1] else {
            panic!("description list should remain a list block");
        };
        assert_eq!(list.items.len(), 4);
        let contents = list
            .items
            .iter()
            .map(|item| match item.blocks.first() {
                Some(ProposalBlock::Markdown(content)) => content.as_str(),
                _ => panic!("list item should retain paragraph content"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            contents,
            vec![
                "Aims to make it easier for others (through a logic contract upgrade) to build & run public broadcaster code, ideally resulting in a more reliable and easier to use broadcaster network.",
                "Updates the cyptographic circuits of RAILGUN to improve gas efficiency, remove redundant code for efficiency purposes, and add additional security features.",
                "Adds new randomness to the circuits via an additional ceremony.",
                "No other changes.",
            ]
        );
    }

    #[test]
    fn proposal_block_parser_keeps_only_effective_referenced_definitions() {
        let source = "[first][guide]\n\n[guide]: https://example.com/first\n[guide]: https://example.com/second\n[unused]: https://example.com/unused\n";
        let blocks = parse_proposal_blocks(source).expect("description should parse");
        assert_eq!(blocks.len(), 1);
        let ProposalBlock::Markdown(content) = &blocks[0] else {
            panic!("reference paragraph should remain a markdown leaf");
        };
        assert!(content.contains("[guide]: https://example.com/first"));
        assert!(!content.contains("https://example.com/second"));
        assert!(!content.contains("https://example.com/unused"));
    }

    #[test]
    fn proposal_block_parser_coalesces_exact_ranges_and_respects_boundaries() {
        let source = "first\n\n[one][guide]\n\n[two][guide]\n\n[guide]: https://example.com/guide\n\n- item\n\n> quote\n\nlast";
        let blocks = parse_proposal_blocks(source).expect("description should parse");
        assert_eq!(blocks.len(), 4);
        let ProposalBlock::Markdown(content) = &blocks[0] else {
            panic!("ordinary nodes should be coalesced into one markdown leaf");
        };
        assert!(content.starts_with("first\n\n"));
        let Node::Root(rendered) =
            to_mdast(content, &ParseOptions::gfm()).expect("coalesced markdown should parse")
        else {
            panic!("coalesced markdown should have a root");
        };
        let visible = super::visible_node_text(&Node::Root(rendered));
        assert!(visible.contains("one (https://example.com/guide)"));
        assert!(visible.contains("two (https://example.com/guide)"));
        assert_eq!(
            content
                .matches("[guide]: https://example.com/guide")
                .count(),
            1
        );
        assert!(matches!(&blocks[1], ProposalBlock::List(_)));
        assert!(matches!(&blocks[2], ProposalBlock::Blockquote(_)));
        assert_eq!(blocks[3], ProposalBlock::Markdown("last".to_string()));
    }

    #[test]
    fn proposal_block_parser_keeps_tables_as_exact_scoped_boundaries() {
        let source = "before\n\n| Name | Link |\n| --- | --- |\n| one | [guide][guide] |\n| two | ![diagram](https://image.example) |\n\n[guide]: https://example.com/guide\n\nafter";
        let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(source) else {
            panic!("table description should prepare");
        };
        assert_eq!(prepared.table_count, 1);
        assert_eq!(prepared.blocks.len(), 3);
        assert_eq!(
            prepared.blocks[0],
            ProposalBlock::Markdown("before".to_string())
        );
        let ProposalBlock::Table(table) = &prepared.blocks[1] else {
            panic!("table should remain a structural block");
        };
        assert_eq!(table.ordinal, 0);
        assert!(table.source.starts_with("| Name | Link |\n| --- | --- |"));
        assert!(
            table
                .render_source
                .starts_with("| Name | Link |\n| --- | --- |")
        );
        let Node::Root(rendered) =
            to_mdast(&table.render_source, &ParseOptions::gfm()).expect("render table parses")
        else {
            panic!("render table should have a root");
        };
        assert!(!super::contains_activatable_markdown(&rendered.children));
        assert!(
            super::visible_node_text(&Node::Root(rendered))
                .contains("diagram (https://image.example)")
        );
        assert_eq!(
            table
                .source
                .matches("[guide]: https://example.com/guide")
                .count(),
            1
        );
        assert_eq!(
            prepared.blocks[2],
            ProposalBlock::Markdown("after".to_string())
        );
    }

    #[test]
    fn proposal_tables_receive_recursive_source_order_ordinals() {
        let source = "| root |\n| --- |\n| one |\n\n> | quote |\n> | --- |\n> | two |\n\n- | list |\n  | --- |\n  | three |\n";
        let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(source) else {
            panic!("nested table description should prepare");
        };
        assert_eq!(prepared.table_count, 3);
        let ProposalBlock::Table(root) = &prepared.blocks[0] else {
            panic!("root table should be first");
        };
        let ProposalBlock::Blockquote(quote) = &prepared.blocks[1] else {
            panic!("quote should remain structural");
        };
        let ProposalBlock::Table(quoted) = &quote.blocks[0] else {
            panic!("quoted table should be second");
        };
        let ProposalBlock::List(list) = &prepared.blocks[2] else {
            panic!("list should remain structural");
        };
        let ProposalBlock::Table(listed) = &list.items[0].blocks[0] else {
            panic!("list table should be third");
        };
        assert_eq!((root.ordinal, quoted.ordinal, listed.ordinal), (0, 1, 2));
        assert_eq!(root.source, "| root |\n| --- |\n| one |");
        for table in [root, quoted, listed] {
            let Node::Root(parsed) = to_mdast(&table.source, &ParseOptions::gfm())
                .expect("prepared table should reparse")
            else {
                panic!("standalone table source should produce a root");
            };
            assert_eq!(parsed.children.len(), 1);
            assert!(matches!(parsed.children.first(), Some(Node::Table(_))));
            assert!(matches!(
                parsed.children.first(),
                Some(Node::Table(table)) if table_fingerprint(table).is_some()
            ));
        }
    }

    #[test]
    fn nested_table_normalization_preserves_inline_content_and_effective_definition() {
        let source = "- > | Name | Details |\n  > | --- | --- |\n  > | *one* | `two` [guide][guide] |\n\n[guide]: https://example.com/guide\n";
        let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(source) else {
            panic!("nested table description should prepare");
        };
        let ProposalBlock::List(list) = &prepared.blocks[0] else {
            panic!("list should remain structural");
        };
        let ProposalBlock::Blockquote(quote) = &list.items[0].blocks[0] else {
            panic!("blockquote should remain structural");
        };
        let ProposalBlock::Table(table) = &quote.blocks[0] else {
            panic!("nested table should remain structural");
        };
        assert_eq!(table.source.matches("[guide]:").count(), 1);
        let Node::Root(rendered) =
            to_mdast(&table.render_source, &ParseOptions::gfm()).expect("render table parses")
        else {
            panic!("render table should have a root");
        };
        assert!(!super::contains_activatable_markdown(&rendered.children));
        let Node::Root(parsed) =
            to_mdast(&table.source, &ParseOptions::gfm()).expect("normalized table should reparse")
        else {
            panic!("normalized table should produce a root");
        };
        assert!(matches!(parsed.children.first(), Some(Node::Table(_))));
        assert!(matches!(parsed.children.get(1), Some(Node::Definition(_))));
        assert_eq!(parsed.children.len(), 2);
    }

    #[test]
    fn proposal_table_intrinsic_width_tracks_content_and_is_bounded() {
        let long_cell = "W".repeat(100);
        let source = format!("| Value |\n| --- |\n| {long_cell} |\n");
        let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(&source)
        else {
            panic!("long-wide table should prepare");
        };
        let ProposalBlock::Table(table) = &prepared.blocks[0] else {
            panic!("long-wide table should remain structural");
        };
        let expected_content_width = 100 * usize::from(APP_TEXT_SIZE.ceil());
        let expected_width =
            expected_content_width + TABLE_COLUMN_CHROME_PX + TABLE_OUTER_BORDER_PX;
        assert!(usize::from(table.render_width_px) >= expected_width);
        assert!(usize::from(table.render_width_px) > usize::from(CONTENT_WIDTH));

        let compact_source = "| Name | Link |\n| --- | --- |\n| one | two |\n";
        let ProposalPresentation::Prepared(prepared) =
            prepare_proposal_presentation(compact_source)
        else {
            panic!("compact table should prepare");
        };
        let ProposalBlock::Table(table) = &prepared.blocks[0] else {
            panic!("compact table should remain structural");
        };
        assert!(usize::from(table.render_width_px) < usize::from(CONTENT_WIDTH));

        let multibyte_source = "| Value |\n| --- |\n| 世界🌍 |\n";
        let ProposalPresentation::Prepared(prepared) =
            prepare_proposal_presentation(multibyte_source)
        else {
            panic!("multibyte table should prepare");
        };
        let ProposalBlock::Table(table) = &prepared.blocks[0] else {
            panic!("multibyte table should remain structural");
        };
        assert!(usize::from(table.render_width_px) <= MAX_TABLE_RENDER_WIDTH_PX);

        let column_count = 40;
        let headers = (0..column_count)
            .map(|column| format!("h{column}"))
            .collect::<Vec<_>>()
            .join(" | ");
        let separators = (0..column_count)
            .map(|_| "---")
            .collect::<Vec<_>>()
            .join(" | ");
        let pathological_cell = "x".repeat(160);
        let cells = (0..column_count)
            .map(|_| pathological_cell.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        let source = format!("| {headers} |\n| {separators} |\n| {cells} |\n");
        let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(&source)
        else {
            panic!("pathological table should prepare");
        };
        let ProposalBlock::Table(table) = &prepared.blocks[0] else {
            panic!("pathological table should remain structural");
        };
        assert_eq!(
            usize::from(table.render_width_px),
            MAX_TABLE_RENDER_WIDTH_PX
        );
    }

    #[test]
    fn proposal_preparation_enforces_render_complexity_at_limit() {
        let mut budget = PreparationBudget::default();
        for _ in 0..85 {
            budget
                .account_markdown(1)
                .expect("85 markdown chunks should use 255 units");
        }
        budget
            .account_list_item()
            .expect("the 256th unit should be admitted");
        assert_eq!(budget.weighted_complexity, 256);
        assert_eq!(
            budget.account_blockquote(),
            Err(ProposalPreparationFailure::TooComplex)
        );
    }

    #[test]
    fn proposal_preparation_accounts_table_complexity_through_production() {
        let source = (0..86)
            .map(|_| "| value |\n| --- |\n\n")
            .collect::<String>();
        assert!(matches!(
            prepare_proposal_presentation(&source),
            ProposalPresentation::TooComplex
        ));
    }

    #[test]
    fn proposal_preparation_rejects_mdast_node_limit_before_building_leaves() {
        let source = "# \n".repeat(MAX_MDAST_NODES);
        let Node::Root(root) = to_mdast(&source, &ParseOptions::gfm()).expect("markdown parses")
        else {
            panic!("markdown parser should produce a root");
        };
        assert_eq!(
            ensure_mdast_node_limit(&root.children),
            Err(ProposalPreparationFailure::TooComplex)
        );

        let source = "# \n".repeat(MAX_MDAST_NODES - 1);
        let Node::Root(root) = to_mdast(&source, &ParseOptions::gfm()).expect("markdown parses")
        else {
            panic!("markdown parser should produce a root");
        };
        assert!(ensure_mdast_node_limit(&root.children).is_ok());
        assert!(matches!(
            prepare_proposal_presentation(&"# \n".repeat(MAX_MDAST_NODES)),
            ProposalPresentation::TooComplex
        ));
    }

    #[test]
    fn proposal_preparation_counts_definition_appendix_bytes_cumulatively() {
        let referenced = ["[guide]: https://example.com/guide"];
        let source_len = MAX_PREPARED_SOURCE_BYTES - 40;
        let prepared_len = prepared_markdown_source_len(source_len, &referenced)
            .expect("source length arithmetic should be checked");
        let mut budget = PreparationBudget::default();
        budget
            .account_markdown(prepared_len)
            .expect("the first prepared leaf should fit");
        assert_eq!(budget.prepared_source_bytes, prepared_len);
        assert_eq!(
            budget.account_markdown(20),
            Err(ProposalPreparationFailure::TooComplex)
        );
    }

    #[test]
    fn proposal_block_parser_prepares_lists_nested_in_blockquotes() {
        let source = "> quoted\n>\n> - outer\n>   - inner\n";
        let blocks = parse_proposal_blocks(source).expect("description should parse");
        let ProposalBlock::Blockquote(quote) = &blocks[0] else {
            panic!("quote should remain a structural block");
        };
        let ProposalBlock::List(list) = &quote.blocks[1] else {
            panic!("quoted list should remain a custom list block");
        };
        let ProposalBlock::List(nested) = &list.items[0].blocks[1] else {
            panic!("nested quoted list should remain a custom list block");
        };
        assert_eq!(nested.items.len(), 1);
    }

    #[test]
    fn proposal_markdown_links_are_rendered_as_inert_visible_content() {
        let source = "[human **label**](https://example.com) [https://label.example](https://destination.example) [https://same.example](https://same.example) [support@example.com](mailto:support@example.com) *[docs at https://example.com](https://destination.example)* *[contact support@example.com](mailto:support@example.com)* https://bare.example <https://autolink.example>\n\n[full][guide] [shortcut] [collapsed][] [![image](https://image.example)](https://wrapper.example)\n\n[guide]: https://reference.example\n[shortcut]: https://shortcut.example\n[collapsed]: https://collapsed.example";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        assert!(rendered.contains("**label**"));
        assert!(rendered.contains("human"));
        assert!(rendered.contains("&#58;"));
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("human label (https://example.com)"));
        assert!(visible.contains("https://label.example (https://destination.example)"));
        assert_eq!(visible.matches("https://same.example").count(), 1);
        assert_eq!(visible.matches("mailto:support@example.com").count(), 1);
        assert!(visible.contains("docs at https://example.com (https://destination.example)"));
        assert!(visible.contains("contact support@example.com"));
        assert!(visible.contains("full (https://reference.example)"));
        assert!(visible.contains("shortcut (https://shortcut.example)"));
        assert!(visible.contains("collapsed (https://collapsed.example)"));
        assert!(visible.contains("image (https://image.example) (https://wrapper.example)"));
        assert_eq!(visible.matches("https://autolink.example").count(), 1);
        assert_eq!(visible.matches("https://bare.example").count(), 1);
    }

    #[test]
    fn proposal_reference_links_use_first_definition_and_deduplicate_equivalent_urls() {
        let source = "[same][same] [same-short] [same-collapsed][] [email][email-ref]\n\n[same]: https://same.example\n[same-short]: https://short.example\n[same-collapsed]: https://collapsed.example\n[email-ref]: mailto:support@example.com\n[same]: https://later.example";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("same (https://same.example)"));
        assert!(!visible.contains("later.example"));
        assert!(visible.contains("same-short (https://short.example)"));
        assert!(visible.contains("same-collapsed (https://collapsed.example)"));
        assert!(visible.contains("email (mailto:support@example.com)"));
    }

    fn contains_markdown_node(nodes: &[Node], predicate: &impl Fn(&Node) -> bool) -> bool {
        nodes.iter().any(|node| {
            predicate(node)
                || node
                    .children()
                    .is_some_and(|children| contains_markdown_node(children, predicate))
        })
    }

    #[test]
    fn proposal_images_are_inert_and_preserve_distinct_destinations() {
        let source = "![direct](https://image.example) ![https://same-image.example](https://same-image.example) [![nested](https://shared.example)](https://shared.example) [![different](https://inner.example)](https://outer.example) ![reference][image-ref]\n\n[image-ref]: https://reference-image.example";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("direct (https://image.example)"));
        assert_eq!(visible.matches("https://same-image.example").count(), 1);
        assert_eq!(visible.matches("https://shared.example").count(), 1);
        assert!(visible.contains("different (https://inner.example) (https://outer.example)"));
        assert!(visible.contains("reference (https://reference-image.example)"));
    }

    #[test]
    fn proposal_destination_equivalence_is_exact_and_mailto_specific() {
        let source = "[mailto:https://label.example](https://label.example) [https://label.example](mailto:https://label.example) [foo](foo) [support@example.com](mailto:support@example.com)";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("mailto:https://label.example (https://label.example)"));
        assert!(visible.contains("https://label.example (mailto:https://label.example)"));
        assert!(visible.contains("foo (foo)"));
        assert_eq!(visible.matches("support@example.com").count(), 1);
        assert!(!visible.contains("mailto:support@example.com"));
    }

    #[test]
    fn proposal_equivalent_url_labels_preserve_safe_formatting() {
        let source = "[**https://strong.example**](https://strong.example) [*https://emphasis.example*](https://emphasis.example) [~~https://delete.example~~](https://delete.example) [`https://code.example`](https://code.example) [https://**mixed**.example](https://mixed.example)";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        assert!(contains_markdown_node(&root.children, &|node| matches!(
            node,
            Node::Strong(_)
        )));
        assert!(contains_markdown_node(&root.children, &|node| matches!(
            node,
            Node::Emphasis(_)
        )));
        assert!(contains_markdown_node(&root.children, &|node| matches!(
            node,
            Node::Delete(_)
        )));
        assert!(contains_markdown_node(&root.children, &|node| matches!(
            node,
            Node::InlineCode(_)
        )));
        let visible = super::visible_node_text(&Node::Root(root));
        for destination in [
            "https://strong.example",
            "https://emphasis.example",
            "https://delete.example",
            "https://code.example",
            "https://mixed.example",
        ] {
            assert_eq!(visible.matches(destination).count(), 1);
        }
    }

    #[test]
    fn proposal_destinations_preserve_decoded_boundary_whitespace() {
        let source = "[inline](<https://example.com/a&#32;>) [reference][space]\n\n[space]: <&#32;https://example.com/b&#32;>";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("inline (https://example.com/a )"));
        assert!(visible.contains("reference ( https://example.com/b )"));
    }

    #[test]
    fn proposal_destination_entities_are_counted_in_prepared_source_budget() {
        let destination = "/".repeat(MAX_PREPARED_SOURCE_BYTES / 4);
        let source = format!("[label](<{destination}>)");
        assert!(matches!(
            prepare_proposal_presentation(&source),
            ProposalPresentation::TooComplex
        ));
    }

    #[test]
    fn proposal_markdown_sanitizes_nested_structures_and_html() {
        let source = "> quote [link](https://example.com)\n>\n> - [nested][ref]\n\n[ref]: https://reference.example\n\n<a href=\"https://example.com\">anchor</a>";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        assert!(rendered.contains("quote link"));
        assert!(rendered.contains("nested"));
        assert!(rendered.contains("anchor"));
        assert!(rendered.contains("`<a href=\"https://example.com\">"));
        assert!(rendered.contains("</a>`"));
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
    }

    #[test]
    fn malformed_proposal_fallback_is_selectable_inert_code() {
        let source = "[not closed\n<a href=\"https://example.com\">";
        let fallback = super::inert_raw_fallback_source(source);
        let Node::Root(root) = to_mdast(&fallback, &ParseOptions::gfm()).expect("fallback parses")
        else {
            panic!("fallback should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        assert!(fallback.contains(source));
    }

    #[test]
    fn table_scroll_handles_are_stable_per_proposal_and_ordinal() {
        let mut state = ProposalsState::new(1);
        let v2 = test_proposal(7, ProposalDocumentState::Pending).identity();
        let mut v1 = v2.clone();
        v1.contract_version = GovernanceContractVersion::V1;
        state.ensure_table_scroll_handles(&v2, 2);
        state.ensure_table_scroll_handles(&v1, 1);
        let first_key = proposal_table_scroll_key(&v2, 0);
        let second_key = proposal_table_scroll_key(&v2, 1);
        let v1_key = proposal_table_scroll_key(&v1, 0);
        assert_ne!(first_key, second_key);
        assert_ne!(first_key, v1_key);
        let first_offset = gpui::point(gpui::px(17.0), gpui::px(-3.0));
        state.table_scroll_handles[&first_key].set_offset(first_offset);
        let first_handle = state.table_scroll_handles[&first_key].clone();
        state.ensure_table_scroll_handles(&v2, 2);
        assert_eq!(state.table_scroll_handles.len(), 3);
        assert_eq!(
            state.table_scroll_handles[&first_key].offset(),
            first_offset
        );
        assert_eq!(
            state.table_scroll_handles[&first_key].offset(),
            first_handle.offset()
        );
    }

    #[test]
    fn calldata_display_preserves_exact_copy_source_and_compact_preview() {
        let calldata = alloy::primitives::Bytes::from(vec![0xab; 40]);
        let exact = action_calldata_hex(&calldata);
        assert_eq!(exact, format!("0x{}", "ab".repeat(40)));
        assert_eq!(
            compact_calldata_display(&exact),
            "0xababababababababab…abababababababab"
        );
    }

    #[test]
    fn action_control_ids_are_unique_per_proposal_ordinal_and_control() {
        let identity = test_proposal(7, resolved_document()).identity();
        let first = proposal_action_id(&identity, 0, "calldata", "clipboard");
        let second = proposal_action_id(&identity, 1, "calldata", "clipboard");
        let display = proposal_action_id(&identity, 0, "calldata", "display");
        assert_ne!(first, second);
        assert_ne!(first, display);
    }

    #[test]
    fn refresh_prunes_action_expansion_only_for_removed_ordinals() {
        let mut state = ProposalsState::new(1);
        let mut source = test_proposal(7, resolved_document());
        source.proposal.actions = vec![
            GovernanceProposalAction {
                call_contract: Address::ZERO,
                calldata: alloy::primitives::Bytes::from(vec![1]),
                value: U256::ZERO,
            },
            GovernanceProposalAction {
                call_contract: Address::ZERO,
                calldata: alloy::primitives::Bytes::from(vec![2]),
                value: U256::ZERO,
            },
        ];
        let identity = source.identity();
        state.pages.insert(
            0,
            ProposalsPage {
                rows: Arc::new(vec![source]),
            },
        );
        state.expanded_calldata.extend([
            ProposalActionIdentity {
                proposal: identity.clone(),
                ordinal: 0,
            },
            ProposalActionIdentity {
                proposal: identity.clone(),
                ordinal: 1,
            },
        ]);
        let mut refreshed = test_proposal(7, ProposalDocumentState::Pending);
        refreshed.proposal.actions = vec![GovernanceProposalAction {
            call_contract: Address::ZERO,
            calldata: alloy::primitives::Bytes::from(vec![1]),
            value: U256::ZERO,
        }];
        state.replace_refreshed_page(0, 0, vec![refreshed]);
        assert!(state.expanded_calldata.contains(&ProposalActionIdentity {
            proposal: identity.clone(),
            ordinal: 0,
        }));
        assert!(!state.expanded_calldata.contains(&ProposalActionIdentity {
            proposal: identity,
            ordinal: 1,
        }));
    }

    #[test]
    fn prefetch_selects_only_the_immediately_next_page() {
        assert_eq!(next_proposal_page(3, 10), Some(4));
        assert_eq!(next_proposal_page(8, 10), Some(9));
        assert_eq!(next_proposal_page(9, 10), None);

        let mut state = ProposalsState::new(1);
        state.current_page = 3;
        state.total_pages = 5;
        state.pages.insert(
            3,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(1, resolved_document())]),
            },
        );
        assert_eq!(state.prefetch_candidate(3), Some(4));

        state.pages.insert(
            3,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(1, ProposalDocumentState::Pending)]),
            },
        );
        assert_eq!(state.prefetch_candidate(3), None);

        state.current_page = 4;
        state.pages.insert(
            4,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(1, resolved_document())]),
            },
        );
        assert_eq!(state.prefetch_candidate(4), None);
    }

    #[test]
    fn rail_amounts_preserve_18_decimal_scaling() {
        let amount = U256::from(1_500_000_000_000_000_000u128);
        assert_eq!(format_compact_rail_amount_with_unit(amount), "1.5 RAIL");

        let rounded = U256::from(12_345_600_000_000_000_000u128);
        assert_eq!(format_compact_rail_amount_with_unit(rounded), "12.35 RAIL");
        assert_eq!(
            format_compact_rail_amount(U256::from(27_000u128) * U256::from(10).pow(U256::from(18))),
            "27K"
        );
        assert_eq!(
            format_compact_rail_amount(
                U256::from(242_879u128) * U256::from(10).pow(U256::from(18))
            ),
            "242.8K"
        );
        assert_eq!(
            format_compact_rail_amount_with_unit(
                U256::from(2_043_001u128) * U256::from(10).pow(U256::from(18))
            ),
            "2.04M RAIL"
        );
    }

    #[test]
    fn vote_split_uses_raw_base_unit_votes() {
        let point_nine_rail = U256::from(900_000_000_000_000_000u128);
        let point_eight_rail = U256::from(800_000_000_000_000_000u128);
        let one_rail = U256::from(1_000_000_000_000_000_000u128);

        assert_eq!(vote_split(U256::ZERO, U256::ZERO), None);
        assert_eq!(vote_split(point_nine_rail, one_rail), Some(473));
        assert_eq!(vote_split(point_nine_rail, point_eight_rail), Some(529));
    }

    #[test]
    fn chain_time_anchor_current_at_uses_sampled_time() {
        let anchor = super::ChainTimeAnchor {
            chain_time: U256::from(100),
            captured_at: Instant::now(),
        };
        assert_eq!(anchor.current_at(0), U256::from(100));
        assert_eq!(anchor.current_at(60), U256::from(160));
    }

    #[test]
    fn list_deadline_tracks_each_voting_phase() {
        let deadlines = GovernanceProposalDeadlines {
            sponsorship: U256::ZERO,
            voting_start: Some(U256::from(10)),
            yay_end: Some(U256::from(20)),
            nay_end: Some(U256::from(30)),
            execution_start: Some(U256::from(40)),
            execution_end: Some(U256::from(50)),
        };
        let mut status = GovernanceProposalStatus {
            stage: GovernanceProposalStage::VotingOpen,
            deadlines,
            quorum_basis: wallet_ops::GovernanceQuorumBasis::AffirmativeOnly,
            quorum: U256::ZERO,
            quorum_progress: U256::ZERO,
            quorum_met: false,
            majority: wallet_ops::GovernanceMajorityResult::Tie,
        };
        assert_eq!(
            list_voting_deadline(&status),
            Some((U256::from(20), "Yay voting ends in"))
        );
        status.stage = GovernanceProposalStage::NayOnlyVoting;
        assert_eq!(
            list_voting_deadline(&status),
            Some((U256::from(30), "Nay voting ends in"))
        );
    }

    #[test]
    fn timeline_deadline_boundaries_match_protocol_semantics() {
        for milestone in [
            TimelineDeadline::SponsorshipClose,
            TimelineDeadline::YayVotingEnd,
            TimelineDeadline::NayVotingEnd,
            TimelineDeadline::ExecutionClose,
        ] {
            assert!(timeline_deadline_completed(
                milestone,
                U256::from(10),
                U256::from(10)
            ));
            assert!(!timeline_deadline_completed(
                milestone,
                U256::from(9),
                U256::from(10)
            ));
        }
        for milestone in [
            TimelineDeadline::VotingOpen,
            TimelineDeadline::ExecutionOpen,
        ] {
            assert!(!timeline_deadline_completed(
                milestone,
                U256::from(10),
                U256::from(10)
            ));
            assert!(timeline_deadline_completed(
                milestone,
                U256::from(11),
                U256::from(10)
            ));
        }
    }

    #[test]
    fn canceled_page_work_token_rejects_reused_page_ownership() {
        let mut state = ProposalsState::new(1);
        state.prefetch_page = Some(1);
        let old_prefetch_token = state.prefetch_token;
        assert!(state.owns_page_work(1, true, old_prefetch_token));
        state.cancel_prefetch();
        state.prefetch_page = Some(1);
        assert!(!state.owns_page_work(1, true, old_prefetch_token));
        assert!(state.owns_page_work(1, true, state.prefetch_token));

        state.current_page = 1;
        let old_active_token = state.active_page_token;
        state.cancel_active_page(1);
        assert!(!state.owns_page_work(1, false, old_active_token));
        assert!(state.owns_page_work(1, false, state.active_page_token));
    }

    #[test]
    fn take_cleanup_invalidates_work_and_preserves_cached_ui_state() {
        let mut state = ProposalsState::new(42);
        state.generation = 7;
        state.request_generation = 11;
        state.active_page_token = 13;
        state.prefetch_token = 17;
        state.checked = true;
        state.overview = Some(GovernanceOverview {
            chain_id: 42,
            v2: GovernanceContractSummary {
                version: GovernanceContractVersion::V2,
                address: Address::ZERO,
                proposal_count: U256::from(10),
                rules: GovernanceContractRules {
                    sponsor_threshold: U256::from(2),
                    quorum: U256::from(1),
                    sponsor_window: U256::from(1),
                    voting_start_offset: U256::from(1),
                    voting_yay_end_offset: U256::from(2),
                    voting_nay_end_offset: U256::from(3),
                    execution_start_offset: U256::from(4),
                    execution_end_offset: U256::from(5),
                },
            },
            v1: None,
        });
        state.total_pages = 2;
        state.current_page = 1;
        state.pages.insert(
            1,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(3, resolved_document())]),
            },
        );
        state.loading_pages.extend([1, 2]);
        state.hydrating_pages.insert(1);
        state.prefetch_page = Some(2);
        state.loading = true;
        state.refreshing = true;
        state.error = Some(Arc::from("cached error"));
        let identity = test_proposal(3, resolved_document()).identity();
        state.selected = Some(ProposalSelection {
            page: 1,
            identity,
            tab: ProposalDetailTab::Actions,
        });

        let old_generation = state.generation;
        let old_request_generation = state.request_generation;
        let old_active_token = state.active_page_token;
        let old_prefetch_token = state.prefetch_token;
        assert!(state.owns_page_work(1, false, old_active_token));
        assert!(state.owns_page_work(2, true, old_prefetch_token));
        let cached_rows = state.pages[&1].rows.clone();
        let overview = state.overview.clone();
        let selected = state.selected.clone();
        let document_semaphore = Arc::clone(&state.document_semaphore);
        let cleanup = state.take_cleanup();

        assert_eq!(state.generation, old_generation.wrapping_add(1));
        assert_eq!(
            state.request_generation,
            old_request_generation.wrapping_add(1)
        );
        assert_eq!(state.active_page_token, old_active_token.wrapping_add(1));
        assert_eq!(state.prefetch_token, old_prefetch_token.wrapping_add(1));
        assert!(!state.owns_page_work(1, false, old_active_token));
        assert!(!state.owns_page_work(2, true, old_prefetch_token));
        assert!(state.loading_pages.is_empty());
        assert!(state.hydrating_pages.is_empty());
        assert_eq!(state.prefetch_page, None);
        assert!(!state.loading);
        assert!(!state.refreshing);
        assert_eq!(state.chain_id, 42);
        assert!(state.checked);
        assert_eq!(state.overview, overview);
        assert_eq!(state.total_pages, 2);
        assert_eq!(state.current_page, 1);
        assert!(Arc::ptr_eq(&state.pages[&1].rows, &cached_rows));
        assert!(Arc::ptr_eq(&state.document_semaphore, &document_semaphore));
        assert_eq!(state.error.as_deref(), Some("cached error"));
        assert_eq!(
            state.selected.as_ref().map(|selected| selected.page),
            selected.as_ref().map(|selected| selected.page)
        );
        assert_eq!(
            state.selected.as_ref().map(|selected| &selected.identity),
            selected.as_ref().map(|selected| &selected.identity)
        );
        assert!(cleanup.tasks.is_empty());
    }

    #[test]
    fn pending_hydration_selection_keeps_only_pending_rows() {
        let mut state = ProposalsState::new(1);
        state.pages.insert(
            2,
            ProposalsPage {
                rows: Arc::new(vec![
                    test_proposal(1, ProposalDocumentState::Pending),
                    test_proposal(2, resolved_document()),
                ]),
            },
        );

        let rows = state
            .pending_document_rows(2)
            .expect("cached page should be selectable");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].has_pending_document());
        assert!(state.pending_document_rows(3).is_none());
    }

    #[test]
    fn pending_rows_distinguish_terminal_and_uncached_pages() {
        let mut state = ProposalsState::new(1);
        state.pages.insert(
            2,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(1, ProposalDocumentState::Pending)]),
            },
        );
        state.pages.insert(
            4,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(2, resolved_document())]),
            },
        );

        assert!(!state.page_is_terminal(2));
        assert!(state.page_is_terminal(4));
        assert!(
            state
                .pending_document_rows(4)
                .is_some_and(|rows| rows.is_empty())
        );
        assert!(state.pending_document_rows(6).is_none());
    }

    #[test]
    fn manual_refresh_keeps_active_page_and_retries_only_unavailable_documents() {
        let mut state = ProposalsState::new(1);
        state.current_page = 3;
        state.pages.insert(
            1,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(1, resolved_document())]),
            },
        );
        state.pages.insert(
            2,
            ProposalsPage {
                rows: Arc::new(vec![
                    test_proposal(10, resolved_document_with_title("cached destination")),
                    test_proposal(12, resolved_document()),
                ]),
            },
        );
        state.pages.insert(
            3,
            ProposalsPage {
                rows: Arc::new(vec![
                    test_proposal(10, resolved_document()),
                    test_proposal(
                        11,
                        ProposalDocumentState::Resolved {
                            document: GovernanceDocument {
                                title: "Document unavailable".to_string(),
                                description: String::new(),
                                available: false,
                            },
                            presentation: ProposalPresentation::RawParseFallback(
                                inert_raw_fallback_source(""),
                            ),
                        },
                    ),
                ]),
            },
        );
        state.loading_pages.extend([1, 2, 3, 4]);
        state.hydrating_pages.extend([2, 3]);
        let selected_identity = test_proposal(10, resolved_document()).identity();
        state.selected = Some(ProposalSelection {
            page: 3,
            identity: selected_identity.clone(),
            tab: ProposalDetailTab::Actions,
        });

        state.prepare_manual_refresh();
        assert!(state.pages[&3].rows[0].document().is_some());
        assert!(state.pages[&3].rows[1].has_pending_document());

        state.replace_refreshed_page(
            3,
            2,
            vec![
                test_proposal(10, ProposalDocumentState::Pending),
                test_proposal(11, ProposalDocumentState::Pending),
                test_proposal(12, ProposalDocumentState::Pending),
            ],
        );

        assert_eq!(state.current_page, 2);
        assert_eq!(state.pages.len(), 1);
        assert!(state.pages.contains_key(&2));
        assert!(!state.pages.contains_key(&1));
        assert!(!state.pages.contains_key(&3));
        assert!(state.loading_pages.is_empty());
        assert!(state.hydrating_pages.is_empty());
        assert_eq!(
            state.selected.as_ref().map(|selected| &selected.identity),
            Some(&selected_identity)
        );
        assert_eq!(
            state.selected.as_ref().map(|selected| selected.page),
            Some(2)
        );
        assert_eq!(
            state.pages[&2].rows[0]
                .document()
                .map(|document| document.title.as_str()),
            Some("resolved")
        );
        assert!(state.pages[&2].rows[1].has_pending_document());
        assert!(state.pages[&2].rows[2].has_pending_document());
    }

    #[tokio::test]
    async fn proposal_cleanup_aborts_and_awaits_held_workers() {
        struct DropSentinel(Arc<AtomicBool>);
        impl Drop for DropSentinel {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let active_dropped = Arc::new(AtomicBool::new(false));
        let prefetch_dropped = Arc::new(AtomicBool::new(false));
        let lifecycle_dropped = Arc::new(AtomicBool::new(false));
        let active_sentinel = DropSentinel(Arc::clone(&active_dropped));
        let prefetch_sentinel = DropSentinel(Arc::clone(&prefetch_dropped));
        let lifecycle_sentinel = DropSentinel(Arc::clone(&lifecycle_dropped));
        let active_worker = tokio::spawn(async move {
            let _sentinel = active_sentinel;
            std::future::pending::<()>().await;
        });
        let prefetch_worker = tokio::spawn(async move {
            let _sentinel = prefetch_sentinel;
            std::future::pending::<()>().await;
        });
        let lifecycle_worker = tokio::spawn(async move {
            let _sentinel = lifecycle_sentinel;
            std::future::pending::<()>().await;
        });
        let mut state = ProposalsState::new(1);
        state.task_tracker.track(lifecycle_worker);
        state.active_page_task_tracker.track(active_worker);
        state.prefetch_task_tracker.track(prefetch_worker);
        let cleanup = state.take_cleanup();
        cleanup
            .shutdown()
            .await
            .expect("cleanup should cancel normally");
        assert!(active_dropped.load(Ordering::Acquire));
        assert!(prefetch_dropped.load(Ordering::Acquire));
        assert!(lifecycle_dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn proposal_cleanup_drops_in_flight_document_futures() {
        struct DropSentinel(Arc<AtomicBool>);
        impl Drop for DropSentinel {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let futures = futures_util::stream::FuturesUnordered::new();
        let future_dropped = Arc::clone(&dropped);
        let future_started = Arc::clone(&started);
        let future_notify = Arc::clone(&notify);
        futures.push(async move {
            future_started.store(true, Ordering::Release);
            future_notify.notify_one();
            let _sentinel = DropSentinel(future_dropped);
            std::future::pending::<DocumentCompletion>().await
        });
        let (completion_tx, _completion_rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = tokio::spawn(send_document_completions(futures, completion_tx));
        notify.notified().await;
        assert!(started.load(Ordering::Acquire));
        assert!(!dropped.load(Ordering::Acquire));

        let mut state = ProposalsState::new(1);
        state.active_page_task_tracker.track(worker);
        let cleanup = state.take_cleanup();
        cleanup
            .shutdown()
            .await
            .expect("cleanup should cancel normally");
        assert!(dropped.load(Ordering::Acquire));
    }
}
