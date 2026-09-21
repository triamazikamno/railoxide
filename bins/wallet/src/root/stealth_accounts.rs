use std::collections::BTreeMap;
use std::future::Future;
use std::ops::Range;
use std::sync::{Arc, Weak};

use alloy::primitives::{Address, U256};
use gpui::{
    AppContext, Context, Entity, Focusable, InteractiveElement as _, IntoElement, ParentElement,
    Render, SharedString, Styled, Task, WeakEntity, Window, div, prelude::FluentBuilder as _,
};
use gpui_component::{
    Disableable, Selectable as _, Sizable,
    button::{ButtonGroup, ButtonVariants},
    input::{InputEvent, InputState},
};
use ui::controls::{app_button, app_input, app_muted_text, app_segment_button, app_strong_text};
use wallet_ops::{
    DesktopPrivateSpendAuthorization, ExecutorAsset, ExecutorOwner, WalletSession,
    vault::{ExecutorOperationId, ExecutorRecord},
};

use super::spend_authorization::{
    SpendAuthorizationIntent, SpendAuthorizationSummary, SpendAuthorizationSummaryRow,
};
use super::{ChainUtxoState, WalletRoot, labeled_field};

mod inspector;
mod observations;
mod overlays;
mod presentation;
mod public_account;
mod recovery;
mod token_picker;
mod view;
use observations::AccountObservations;
use recovery::{RecoveryAuthorization, RecoveryForm};
use view::AccountFilter;

pub(super) struct StealthAccountsPanel {
    session: Arc<WalletSession>,
    view: Entity<StealthAccountsView>,
    open: bool,
}

#[derive(Clone)]
pub(super) struct StealthAccountTarget {
    session: Weak<WalletSession>,
    operation: ExecutorOperationId,
}

impl StealthAccountTarget {
    pub(super) fn new(session: &Arc<WalletSession>, operation: ExecutorOperationId) -> Self {
        Self {
            session: Arc::downgrade(session),
            operation,
        }
    }
}

pub(super) struct StealthAuthorization {
    session: Arc<WalletSession>,
    action: StealthAction,
}

#[derive(Clone)]
enum StealthAction {
    Discover { range: Range<u32> },
    Recover(RecoveryAuthorization),
    AddToPublic { operation: ExecutorOperationId },
}

impl StealthAuthorization {
    pub(super) fn hardware_executor_action(&self) -> wallet_ops::HardwareExecutorAction {
        use wallet_ops::HardwareExecutorAction;
        match &self.action {
            StealthAction::Discover { range } => HardwareExecutorAction::Restore(range.clone()),
            StealthAction::AddToPublic { operation } => {
                HardwareExecutorAction::Register(*operation)
            }
            StealthAction::Recover(recovery) => match recovery {
                RecoveryAuthorization::Retry { prepared } => HardwareExecutorAction::Retry {
                    operation: prepared.operation(),
                    transaction: prepared.original().hash(),
                },
                RecoveryAuthorization::Prepare { approval } => {
                    HardwareExecutorAction::Recover(approval.operation())
                }
                RecoveryAuthorization::Submit { prepared, .. } => {
                    HardwareExecutorAction::RecoverPrepared {
                        operation: prepared.operation(),
                        recovery: prepared.recovery(),
                    }
                }
            },
        }
    }

    pub(super) const fn session(&self) -> &Arc<WalletSession> {
        &self.session
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AssetKind {
    Native,
    Erc20,
    Erc721,
}

pub(super) struct StealthAccountsView {
    root: WeakEntity<WalletRoot>,
    session: Arc<WalletSession>,
    owner: Arc<ExecutorOwner>,
    runtime: tokio::runtime::Handle,
    active: bool,
    records: Vec<ExecutorRecord>,
    records_error: Option<String>,
    observations: BTreeMap<ExecutorOperationId, AccountObservations>,
    search: Entity<InputState>,
    search_query: String,
    filter: AccountFilter,
    opened: bool,
    show_hidden: bool,
    expanded: Option<ExecutorOperationId>,
    add_token_focus: gpui::FocusHandle,
    pages: view::AccountPages,
    breadcrumb_focus: gpui::FocusHandle,
    recover_focus: gpui::FocusHandle,
    selected: Option<ExecutorOperationId>,
    recovery: RecoveryForm,
    range_start: Entity<InputState>,
    range_count: Entity<InputState>,
    asset_kind: AssetKind,
    token_address: Entity<InputState>,
    token_id: Entity<InputState>,
    pending_authorization: Option<Arc<StealthAuthorization>>,
    job: Option<tokio::task::AbortHandle>,
    job_revision: u64,
    error: Option<String>,
    coverage: Option<String>,
    observation: Option<Task<()>>,
}

impl Drop for StealthAccountsView {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.abort();
        }
    }
}

impl WalletRoot {
    fn stealth_session(&self) -> Option<Arc<WalletSession>> {
        match self.chain_states.get(&self.selected_chain) {
            Some(
                ChainUtxoState::Ready { session, .. } | ChainUtxoState::Syncing { session, .. },
            ) if session.executor_owner().is_some() => Some(Arc::clone(session)),
            _ => None,
        }
    }

    pub(super) fn stealth_session_is_current(&self, session: &Arc<WalletSession>) -> bool {
        self.stealth_session()
            .is_some_and(|current| Arc::ptr_eq(&current, session))
    }

    pub(super) fn can_open_stealth_account(
        &self,
        target: &StealthAccountTarget,
        cx: &gpui::App,
    ) -> bool {
        target
            .session
            .upgrade()
            .is_some_and(|session| self.stealth_session_is_current(&session))
            && self.stealth_accounts.as_ref().is_none_or(|panel| {
                !self.stealth_session_is_current(&panel.session)
                    || panel.view.read(cx).job.is_none()
            })
    }

    pub(super) fn open_stealth_account(
        &mut self,
        target: &StealthAccountTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.can_open_stealth_account(target, cx) {
            return;
        }
        self.select_wallet_tab(super::WalletTab::Public, cx);
        // The requested account owns focus, rather than the ordinary account search.
        self.focus_public_account_search_on_render = false;
        self.open_stealth_accounts(window, cx);
        if let Some(panel) = &self.stealth_accounts {
            let view = panel.view.clone();
            let operation = target.operation;
            window.defer(cx, move |window, cx| {
                view.update(cx, |view, cx| {
                    if !view.session_is_current(cx) {
                        return;
                    }
                    view.reload_records();
                    view.reveal_account(operation, window, cx);
                });
            });
        }
        cx.notify();
    }

    pub(super) fn render_stealth_accounts_section(&self, root: &Entity<Self>) -> gpui::Div {
        let section = div().w_full().min_w_0();
        if let Some(panel) = &self.stealth_accounts
            && self.stealth_session_is_current(&panel.session)
        {
            return section.child(view::StealthSummary {
                view: panel.view.clone(),
                root: root.clone(),
            });
        }
        section.child(app_muted_text(
            "Stealth accounts will be available after the wallet’s chain session starts.",
        ))
    }

    pub(super) fn stealth_accounts_body(&self) -> Option<Entity<StealthAccountsView>> {
        self.stealth_accounts
            .as_ref()
            .filter(|panel| panel.open && self.stealth_session_is_current(&panel.session))
            .map(|panel| panel.view.clone())
    }

    pub(super) fn ensure_stealth_accounts(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(session) = self
            .stealth_session()
            .filter(|_| self.view_session.is_some())
        else {
            self.clear_stealth_accounts(cx);
            return;
        };
        if self
            .stealth_accounts
            .as_ref()
            .is_some_and(|panel| Arc::ptr_eq(&panel.session, &session))
        {
            return;
        }
        self.clear_stealth_accounts(cx);
        let owner = session.executor_owner().expect("checked executor session");
        let root = cx.entity().downgrade();
        let runtime = self.runtime.clone();
        let view = cx.new(|cx| {
            StealthAccountsView::new(root, Arc::clone(&session), owner, runtime, window, cx)
        });
        cx.observe(&view, |_, _, cx| cx.notify()).detach();
        self.stealth_accounts = Some(StealthAccountsPanel {
            session,
            view,
            open: false,
        });
    }

    pub(super) fn clear_stealth_accounts(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(panel) = self.stealth_accounts.take() {
            panel.view.update(cx, |view, cx| {
                view.active = false;
                if let Some(job) = view.job.take() {
                    job.abort();
                }
                view.job_revision = view.job_revision.wrapping_add(1);
                view.observation = None;
                view.finish_recovery_progress();
                view.close_recovery();
                view.pending_authorization = None;
                view.recovery.prepared = None;
                view.observations.clear();
                view.records.clear();
                view.records_error = None;
                view.search_query.clear();
                view.selected = None;
                view.expanded = None;
                view.coverage = None;
                view.error = None;
                view.pages = view::AccountPages::default();
                cx.notify();
            });
        }
    }

    fn open_stealth_accounts(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.ensure_stealth_accounts(window, cx);
        if let Some(panel) = &mut self.stealth_accounts {
            panel.open = true;
            let view = panel.view.clone();
            window.defer(cx, move |window, cx| {
                view.update(cx, |view, cx| {
                    if !view.session_is_current(cx) {
                        return;
                    }
                    if !view.opened {
                        view.filter = if view
                            .records
                            .iter()
                            .any(|record| view.needs_attention(record))
                        {
                            AccountFilter::Attention
                        } else {
                            AccountFilter::All
                        };
                        view.opened = true;
                    }
                    view.search.read(cx).focus_handle(cx).focus(window, cx);
                    view.refresh_visible(cx);
                    cx.notify();
                });
            });
        }
        cx.notify();
    }
}

impl StealthAccountsView {
    fn new(
        root: WeakEntity<WalletRoot>,
        session: Arc<WalletSession>,
        owner: Arc<ExecutorOwner>,
        runtime: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let mut input = |value: &str| {
            cx.new(|cx| {
                let mut input = InputState::new(window, cx);
                input.set_value(value.to_owned(), window, cx);
                input
            })
        };
        let range_start = input("0");
        let range_count = input("64");
        let token_address = input("");
        let token_id = input("");
        let search = input("");
        search.update(cx, |input, cx| {
            input.set_placeholder("Index, address, purpose, token", window, cx);
        });
        cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.refresh_visible(cx);
                cx.notify();
            }
        })
        .detach();
        for input in [&range_start, &range_count, &token_address, &token_id] {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.pending_authorization = None;
                    this.recovery.prepared = None;
                    this.error = None;
                    cx.notify();
                }
            })
            .detach();
        }
        let (records, records_error) = match owner.records() {
            Ok(records) => (records, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        let mut changes = owner.subscribe();
        let mut private_changes = session.observation_rx.clone();
        let observation = cx.spawn(async move |this, cx| {
            loop {
                let changed = tokio::select! {
                    changed = changes.changed() => changed,
                    changed = private_changes.changed() => changed,
                };
                if changed.is_err() {
                    break;
                }
                if this
                    .update(cx, |this, cx| {
                        if !this.session_is_current(cx) {
                            return;
                        }
                        this.reload_records();
                        this.refresh_visible(cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            root,
            session,
            owner,
            runtime,
            active: true,
            records,
            records_error,
            observations: BTreeMap::new(),
            search,
            search_query: String::new(),
            filter: AccountFilter::All,
            opened: false,
            show_hidden: false,
            expanded: None,
            add_token_focus: cx.focus_handle(),
            pages: view::AccountPages::default(),
            breadcrumb_focus: cx.focus_handle(),
            recover_focus: cx.focus_handle(),
            selected: None,
            recovery: RecoveryForm::new(window, cx),
            range_start,
            range_count,
            asset_kind: AssetKind::Native,
            token_address,
            token_id,
            pending_authorization: None,
            job: None,
            job_revision: 0,
            error: None,
            coverage: None,
            observation: Some(observation),
        }
    }

    fn reload_records(&mut self) {
        match self.owner.records() {
            Ok(records) => {
                self.records = records;
                self.records_error = None;
            }
            Err(error) => self.records_error = Some(error.to_string()),
        }
    }

    fn selected_asset(&self, cx: &gpui::App) -> Result<ExecutorAsset, String> {
        if self.asset_kind == AssetKind::Native {
            return Ok(ExecutorAsset::Native);
        }
        let token = self
            .token_address
            .read(cx)
            .value()
            .trim()
            .parse::<Address>()
            .map_err(|_| "Enter a valid token or collection address.".to_owned())?;
        match self.asset_kind {
            AssetKind::Native => Ok(ExecutorAsset::Native),
            AssetKind::Erc20 => Ok(ExecutorAsset::Erc20(token)),
            AssetKind::Erc721 => Ok(ExecutorAsset::Erc721 {
                collection: token,
                token_id: self
                    .token_id
                    .read(cx)
                    .value()
                    .trim()
                    .parse::<U256>()
                    .map_err(|_| "Enter the NFT token ID.".to_owned())?,
            }),
        }
    }

    fn session_is_current(&self, cx: &gpui::App) -> bool {
        self.active
            && self
                .root
                .upgrade()
                .is_some_and(|root| root.read(cx).stealth_session_is_current(&self.session))
    }

    fn check_record(
        &mut self,
        operation: ExecutorOperationId,
        assets: Vec<ExecutorAsset>,
        cx: &mut Context<'_, Self>,
    ) {
        if self.job.is_some() || !self.session_is_current(cx) {
            return;
        }
        let mut assets = assets;
        if !assets.contains(&ExecutorAsset::Native) {
            assets.push(ExecutorAsset::Native);
        }
        self.observations
            .entry(operation)
            .or_default()
            .begin(&assets);
        let owner = Arc::clone(&self.owner);
        self.start_job(
            async move { Ok((owner.inspect_record(operation, &assets).await, assets)) },
            move |this, (result, assets)| {
                let observations = this.observations.entry(operation).or_default();
                match result {
                    Ok(inspection) => {
                        observations.finish(&inspection, std::time::SystemTime::now());
                    }
                    Err(error) => {
                        observations.fail(&assets, std::time::SystemTime::now());
                        this.error = Some(error.to_string());
                    }
                }
            },
            cx,
        );
    }

    fn request_discovery(
        &mut self,
        range: Range<u32>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.job.is_some() || !self.session_is_current(cx) {
            return;
        }
        let summary = SpendAuthorizationSummary::new("Restore stealth accounts", "Derives these accounts and checks whether they have been used. This does not sign or submit a transaction.", vec![
            SpendAuthorizationSummaryRow::new("Chain", railgun_ui::chain_name(self.session.chain_id).map_or_else(|| self.session.chain_id.to_string(), str::to_owned)),
            SpendAuthorizationSummaryRow::new("Account indices", format!("{}–{}", range.start, range.end - 1)),
            SpendAuthorizationSummaryRow::new("Sent as", format!("One request with all {} addresses", range.end - range.start)),
            SpendAuthorizationSummaryRow::new("Endpoint learns", "These addresses belong together, Tor or not"),
            SpendAuthorizationSummaryRow::new("Checks", "Use only. Balances and history are not read"),
        ]).with_confirm_label("Restore").requiring_explicit_review();
        self.request_authorization(StealthAction::Discover { range }, summary, window, cx);
    }

    fn request_authorization(
        &mut self,
        action: StealthAction,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let command = Arc::new(StealthAuthorization {
            session: Arc::clone(&self.session),
            action,
        });
        self.pending_authorization = Some(Arc::clone(&command));
        let view = cx.entity();
        let _ = self.root.update(cx, |root, cx| {
            if root.stealth_session_is_current(&command.session) {
                root.request_spend_authorization(
                    SpendAuthorizationIntent::StealthAccounts(view, command),
                    summary.requiring_explicit_review(),
                    window,
                    cx,
                );
            }
        });
    }

    pub(super) fn cancel_authorization(
        &mut self,
        command: &Arc<StealthAuthorization>,
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

    pub(super) fn continue_authorized(
        &mut self,
        command: Arc<StealthAuthorization>,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .pending_authorization
            .take()
            .is_some_and(|current| Arc::ptr_eq(&current, &command))
        {
            return;
        }
        if !self.session_is_current(cx) {
            return;
        }
        if let StealthAction::AddToPublic { operation } = command.action {
            self.continue_public_registration(operation, authorization, window, cx);
            return;
        }
        let StealthAction::Discover { range } = command.action.clone() else {
            let StealthAction::Recover(action) = command.action.clone() else {
                unreachable!()
            };
            self.continue_recovery(action, authorization, window, cx);
            return;
        };
        let owner = Arc::clone(&self.owner);
        self.coverage = Some(format!(
            "Checking use for {} accounts… Saved accounts survive failed checks or Stop.",
            range.end - range.start
        ));
        self.start_job(async move {
            owner.discover_authorized_range(&command.session, &authorization, range).await
        }, move |this, report| {
            this.coverage = Some(format!("Restored account indices {}–{}: {} used · {} unused · {} unavailable. Balances have not been checked by Restore.",
                report.range().start, report.range().end - 1,
                report.used(), report.unused(), report.unavailable()));
        }, cx);
    }

    fn start_job<T: Send + 'static>(
        &mut self,
        work: impl Future<Output = eyre::Result<T>> + Send + 'static,
        apply: impl FnOnce(&mut Self, T) + 'static,
        cx: &mut Context<'_, Self>,
    ) {
        if self.job.is_some() {
            return;
        }
        self.error = None;
        self.job_revision = self.job_revision.wrapping_add(1);
        let revision = self.job_revision;
        let join = self.runtime.spawn(work);
        self.job = Some(join.abort_handle());
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let _ = this.update(cx, |this, cx| {
                if this.job_revision != revision || !this.session_is_current(cx) { return; }
                this.job = None;
                this.finish_recovery_progress();
                match result {
                    Ok(Ok(value)) => apply(this, value),
                    Ok(Err(error)) => { this.coverage = None; this.error = Some(error.to_string()); },
                    Err(error) if !error.is_cancelled() => this.error = Some("The local operation stopped unexpectedly. Check the account before retrying.".into()),
                    Err(_) => {},
                }
                this.reload_records();
                this.refresh_visible(cx);
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn render_asset_fields(
        &self,
        include_native: bool,
        token_picker: Option<&Entity<token_picker::TokenPicker>>,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let busy = self.job.is_some();
        let mut fields = div().w_full().min_w_0().flex().flex_col().gap_2().child(
            ButtonGroup::new("stealth-asset-kind")
                .w_full()
                .outline()
                .small()
                .children(
                    [
                        (AssetKind::Native, "Native", "native"),
                        (AssetKind::Erc20, "ERC-20", "erc20"),
                        (AssetKind::Erc721, "NFT (ERC-721)", "erc721"),
                    ]
                    .into_iter()
                    .filter(|(kind, _, _)| include_native || *kind != AssetKind::Native)
                    .enumerate()
                    .map(|(index, (kind, label, id))| {
                        let selected = self.asset_kind == kind;
                        app_button(id, label)
                            .flex_1()
                            .min_w_0()
                            .selected(selected)
                            .disabled(busy)
                            .when(selected, ButtonVariants::primary)
                            // Match the Public mode group's shared selected border.
                            .when(selected && index > 0, |button| {
                                button.border_l_1().ml(-gpui::px(1.))
                            })
                            .debug_selector(move || format!("stealth-asset-kind-{id}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.asset_kind = kind;
                                this.pending_authorization = None;
                                this.recovery.prepared = None;
                                this.error = None;
                                cx.notify();
                            }))
                    }),
                ),
        );
        if self.asset_kind == AssetKind::Erc20
            && let Some(picker) = token_picker
        {
            fields = fields.child(picker.clone());
        } else if self.asset_kind != AssetKind::Native {
            fields = fields.child(labeled_field(
                "Token or collection address",
                app_input(&self.token_address).small().disabled(busy),
            ));
        }
        if self.asset_kind == AssetKind::Erc721 {
            fields = fields.child(labeled_field(
                "Token ID",
                app_input(&self.token_id).small().disabled(busy),
            ));
        }
        fields
    }
}

fn asset_label(asset: ExecutorAsset) -> String {
    match asset {
        ExecutorAsset::Native => "Native currency".into(),
        ExecutorAsset::Erc20(token) => format!("ERC-20 {token}"),
        ExecutorAsset::Erc721 {
            collection,
            token_id,
        } => format!("NFT {collection} #{token_id}"),
    }
}
