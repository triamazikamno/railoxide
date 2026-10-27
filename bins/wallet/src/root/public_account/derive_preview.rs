//! Derive from private for software wallets: one authorization opens an in-memory address
//! preview, and the chosen indexes are added from it in one vault write. The preview can also
//! check the native balances of its selected indexes on request.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write as _,
    rc::Weak,
    sync::Arc,
};

use alloy::primitives::U256;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement, Pixels, SharedString, StatefulInteractiveElement as _, Styled, Subscription,
    Window, div, prelude::FluentBuilder as _, px, rems,
};
use gpui_component::{
    Disableable, IconName, Sizable,
    alert::Alert,
    button::ButtonVariants,
    checkbox::Checkbox,
    description_list::{DescriptionItem, DescriptionList},
    dialog::Confirm,
    input::InputState,
    select::{SelectEvent, SelectState},
};
use railgun_ui::short_address;
use rand::seq::SliceRandom as _;
use ui::chain_select::{ChainSelectItem, ChainSelectItems, chain_select, chain_select_state};
use ui::controls::{app_button, app_input, app_muted_text, app_text};
use wallet_ops::WalletNetworkMode;
use wallet_ops::settings::EffectiveChainConfig;
use wallet_ops::vault::{
    DerivedPublicAccountPreview, PreviewedPublicAccountSkip, PreviewedPublicAccountsAdded,
    PublicAccountScope, PublicAccountSource, PublicAccountStatus, VaultError,
};
use zeroize::Zeroizing;

use crate::root::device_auth::masked_input_with_device_auth;
use crate::root::dialogs::PublicAccountDialogKind;
use crate::root::public_balances::isolated_read_jitter;
use crate::root::ui_helpers::{count_label, input_enter_scope};
use crate::root::vault::{vault_error_kind, vault_error_message};
use crate::root::{WalletRoot, new_text_input};

#[cfg(test)]
mod tests;

const PAGE_SIZE: u32 = DerivedPublicAccountPreview::PAGE_SIZE;
const INDEX_LIMIT: u32 = DerivedPublicAccountPreview::INDEX_LIMIT;

pub(in crate::root) struct DerivePreviewState {
    pub(super) go_to_input: Entity<InputState>,
    /// Holds focus in the open preview so Enter reaches Add.
    pub(super) focus: FocusHandle,
    /// The open Derive dialog's lease.
    pub(super) lease: Weak<Cell<bool>>,
    /// Advanced by every authorization and reset, so a late result can't open a later preview.
    pub(super) authorization: u64,
    pub(super) authorizing: bool,
    /// Advanced by every balance check start, stop and reset, so a stopped check sends no
    /// further address and its in-flight result is discarded.
    pub(super) check_run: u64,
    pub(super) preview: Option<DerivePreview>,
}

/// An authorized preview. Its key links every derived address, so it lives only as long as
/// the dialog's authorization.
pub(in crate::root) struct DerivePreview {
    pub(super) keys: DerivedPublicAccountPreview,
    pub(super) page_start: u32,
    pub(super) selected: BTreeSet<u32>,
    pub(super) go_to_error: Option<Arc<str>>,
    pub(super) last_add: Option<PreviewedPublicAccountsAdded>,
    /// The chain a balance check reads. Changing it sends nothing.
    pub(super) chain_id: u64,
    pub(super) chain_select: Entity<SelectState<ChainSelectItems>>,
    _chain_changes: Subscription,
    /// The summary shown before a check sends anything is open.
    pub(super) reviewing_check: bool,
    /// The indexes of the running check.
    pub(super) checking: Option<Vec<u32>>,
    /// Balance checks by chain ID and index. A missing entry is unchecked. Kept only here.
    pub(super) results: HashMap<(u64, u32), BalanceCheck>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BalanceCheck {
    Queued,
    Loading,
    Done(U256),
    Failed,
}

impl DerivePreviewState {
    pub(in crate::root) fn new(window: &mut Window, cx: &mut Context<'_, WalletRoot>) -> Self {
        Self {
            go_to_input: new_text_input(window, cx, "Go to index"),
            focus: cx.focus_handle(),
            lease: Weak::new(),
            authorization: 0,
            authorizing: false,
            check_run: 0,
            preview: None,
        }
    }

    /// Returns to the authorize state and orphans any pending authorization or balance check.
    pub(in crate::root) fn reset(&mut self) {
        self.authorization = self.authorization.wrapping_add(1);
        self.check_run = self.check_run.wrapping_add(1);
        self.authorizing = false;
        self.preview = None;
    }

    /// Stops a balance check before its next address. The address in progress may finish, but
    /// its result is discarded, and every row without a result returns to unchecked.
    fn stop_balance_check(&mut self) {
        self.check_run = self.check_run.wrapping_add(1);
        if let Some(preview) = self.preview.as_mut() {
            preview.checking = None;
            preview
                .results
                .retain(|_, check| matches!(check, BalanceCheck::Done(_) | BalanceCheck::Failed));
        }
    }
}

const fn page_start(index: u32) -> u32 {
    index - index % PAGE_SIZE
}

/// The most indexes the summary lists before summarizing the rest.
const SUMMARY_LISTED_INDEXES: usize = 8;

/// The rows and warning of the summary shown before a balance check of `indexes` sends any
/// address.
fn balance_check_summary(
    chain: &EffectiveChainConfig,
    mode: WalletNetworkMode,
    indexes: &BTreeSet<u32>,
) -> ([(&'static str, String); 5], &'static str) {
    let count = indexes.len();
    let noun = if count == 1 { "address" } else { "addresses" };
    let mut listed = indexes
        .iter()
        .take(SUMMARY_LISTED_INDEXES)
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if count > SUMMARY_LISTED_INDEXES {
        let _ = write!(listed, " and {} more", count - SUMMARY_LISTED_INDEXES);
    }
    // The mean 0.5–3 s wait between addresses plus about 2 s for each address's reads.
    let seconds = (count as u64 * 3_750).div_ceil(1_000);
    let time = if seconds < 60 {
        format!("About {seconds} s")
    } else {
        format!("About {} min", (seconds + 30) / 60)
    };
    let (route, warning) = match mode {
        WalletNetworkMode::Tor => (
            "Each address on its own Tor route to a randomly chosen provider",
            "RPC providers receive these addresses and can still link them by timing. Tor doesn't prevent that.",
        ),
        WalletNetworkMode::Proxy => (
            "Over your proxy to a randomly chosen provider. The addresses aren't isolated from each other",
            "RPC providers receive these addresses and can link them to each other and to you. The proxy doesn't prevent that.",
        ),
        WalletNetworkMode::Direct => (
            "Directly to a randomly chosen provider. The addresses aren't isolated from each other",
            "RPC providers receive these addresses and can link them to each other and to you.",
        ),
    };
    (
        [
            ("Chain", chain.name.clone()),
            ("Addresses", format!("{listed} ({count} {noun})")),
            (
                "Sent as",
                "One request per address, in random order, 0.5–3 s apart".to_owned(),
            ),
            ("Route", route.to_owned()),
            ("Expected time", time),
        ],
        warning,
    )
}

fn add_result_message(result: &PreviewedPublicAccountsAdded) -> String {
    let mut message = format!("Added {}.", count_label(result.added.len(), "account"));
    for (index, reason) in &result.skipped {
        let reason = match reason {
            PreviewedPublicAccountSkip::AlreadyAdded => "already added",
            PreviewedPublicAccountSkip::DuplicateAddress => "address already in Public accounts",
        };
        let _ = write!(message, " Skipped index {index}: {reason}.");
    }
    message
}

impl WalletRoot {
    /// Authorizes the preview with the typed vault password or one from device authentication.
    pub(super) fn authorize_derive_preview(
        &mut self,
        device_auth_password: Option<Zeroizing<String>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &self.public_form.derive_preview;
        if state.authorizing
            || state.preview.is_some()
            || !state.lease.upgrade().is_some_and(|open| open.get())
        {
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            self.public_form.error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            self.public_form.error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return;
        };
        let password = match device_auth_password {
            Some(password) => password,
            None => Self::read_and_clear_input(&self.public_form.add_password_input, window, cx),
        };
        if password.trim().is_empty() {
            self.public_form.error = Some(Arc::from("Enter the vault password to show addresses"));
            cx.notify();
            return;
        }
        let protected_seed_session = self.protected_software_seed_session.clone();
        let generation = self.active_wallet_generation;
        let state = &mut self.public_form.derive_preview;
        state.authorization = state.authorization.wrapping_add(1);
        state.authorizing = true;
        let authorization = state.authorization;
        let lease = state.lease.clone();
        self.public_form.error = None;
        // The vault KDF is slow, so it runs off the UI thread.
        let join = self.runtime.spawn_blocking(move || {
            store.derived_public_account_preview(
                password.as_str(),
                &view_session,
                protected_seed_session.as_deref(),
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.finish_derive_preview_authorization(
                    authorization,
                    generation,
                    &lease,
                    result,
                    window,
                    cx,
                );
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn finish_derive_preview_authorization(
        &mut self,
        authorization: u64,
        generation: u64,
        lease: &Weak<Cell<bool>>,
        result: Result<Result<DerivedPublicAccountPreview, VaultError>, tokio::task::JoinError>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        if state.authorization != authorization {
            return;
        }
        state.authorizing = false;
        cx.notify();
        if self.active_wallet_generation != generation
            || !lease.upgrade().is_some_and(|open| open.get())
        {
            return;
        }
        match result {
            Ok(Ok(keys)) => {
                let next = self
                    .public_form
                    .next_derived_index
                    .filter(|index| *index < INDEX_LIMIT);
                let chain_id = self.selected_chain;
                let chains = self
                    .effective_chain_configs
                    .enabled_chains()
                    .map(|chain| ChainSelectItem {
                        chain_id: chain.chain_id,
                        label: chain.name.clone().into(),
                    })
                    .collect();
                let chain_select =
                    cx.new(|cx| chain_select_state(chains, Some(chain_id), window, cx));
                let chain_changes = cx.subscribe_in(
                    &chain_select,
                    window,
                    |root, _, event: &SelectEvent<ChainSelectItems>, window, cx| {
                        if let SelectEvent::Confirm(Some(chain_id)) = event {
                            root.set_derive_check_chain(*chain_id, window, cx);
                        }
                    },
                );
                state.preview = Some(DerivePreview {
                    keys,
                    page_start: next.map_or(0, page_start),
                    selected: next.into_iter().collect(),
                    go_to_error: None,
                    last_add: None,
                    chain_id,
                    chain_select,
                    _chain_changes: chain_changes,
                    reviewing_check: false,
                    checking: None,
                    results: HashMap::new(),
                });
                state.focus.focus(window, cx);
                self.public_form.error = None;
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    "derived public account preview failed"
                );
                self.public_form.error = Some(vault_error_message(&error));
            }
            Err(error) => {
                tracing::warn!(%error, "derived public account preview task failed");
                self.public_form.error = Some(Arc::from("Showing addresses failed. Try again."));
            }
        }
    }

    /// The status of each derivation index used by this wallet's derived accounts.
    fn derived_index_statuses(&self) -> BTreeMap<u32, PublicAccountStatus> {
        let mut statuses = BTreeMap::new();
        let Some(wallet_id) = self
            .view_session
            .as_ref()
            .map(|session| session.wallet_id())
        else {
            return statuses;
        };
        for account in &self.public_accounts {
            let (Some(index), PublicAccountScope::PrivateWallet { wallet_uuid }) =
                (account.derivation_index, &account.scope)
            else {
                continue;
            };
            if wallet_uuid != wallet_id
                || !matches!(
                    account.source,
                    PublicAccountSource::Derived | PublicAccountSource::HardwareDerived
                )
            {
                continue;
            }
            let status = statuses.entry(index).or_insert(account.status);
            if account.status == PublicAccountStatus::Active {
                *status = PublicAccountStatus::Active;
            }
        }
        statuses
    }

    /// Selection and paging return focus to the preview, so Enter keeps adding.
    pub(super) fn toggle_derive_preview_index(
        &mut self,
        index: u32,
        checked: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        let Some(preview) = state.preview.as_mut() else {
            return;
        };
        if checked {
            if preview.selected.len() < DerivedPublicAccountPreview::MAX_ADD_INDEXES {
                preview.selected.insert(index);
            }
        } else {
            preview.selected.remove(&index);
        }
        state.focus.focus(window, cx);
        cx.notify();
    }

    /// Selects every Available index on the visible page, in order until the cap, or clears them.
    pub(super) fn select_derive_preview_page(
        &mut self,
        select: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let statuses = self.derived_index_statuses();
        let state = &mut self.public_form.derive_preview;
        let Some(preview) = state.preview.as_mut() else {
            return;
        };
        let start = preview.page_start;
        let end = start.saturating_add(PAGE_SIZE).min(INDEX_LIMIT);
        for index in (start..end).filter(|index| !statuses.contains_key(index)) {
            if !select {
                preview.selected.remove(&index);
            } else if preview.selected.len() < DerivedPublicAccountPreview::MAX_ADD_INDEXES {
                preview.selected.insert(index);
            }
        }
        state.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn show_derive_preview_page(
        &mut self,
        start: u32,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        let Some(preview) = state.preview.as_mut() else {
            return;
        };
        if start < INDEX_LIMIT {
            preview.page_start = page_start(start);
            preview.go_to_error = None;
        }
        state.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn set_derive_check_chain(
        &mut self,
        chain_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        if state
            .preview
            .as_ref()
            .is_none_or(|preview| preview.chain_id == chain_id)
        {
            return;
        }
        state.stop_balance_check();
        if let Some(preview) = state.preview.as_mut() {
            preview.chain_id = chain_id;
        }
        state.focus.focus(window, cx);
        cx.notify();
    }

    /// Opens or closes the summary shown before a balance check. Opening it sends nothing.
    pub(super) fn review_derive_balance_check(
        &mut self,
        open: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        if let Some(preview) = state.preview.as_mut() {
            preview.reviewing_check = open && preview.checking.is_none();
        }
        state.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn stop_derive_balance_check(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        state.stop_balance_check();
        state.focus.focus(window, cx);
        cx.notify();
    }

    /// Checks the selected indexes' native balances on the selected chain, one address at a
    /// time in random order with a random wait between them. Later selection edits and page
    /// changes don't affect a running check.
    pub(super) fn start_derive_balance_check(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        state.focus.focus(window, cx);
        cx.notify();
        let Some(preview) = state.preview.as_mut() else {
            return;
        };
        preview.reviewing_check = false;
        let Some(chain) = self.effective_chain_configs.get(preview.chain_id).cloned() else {
            return;
        };
        if preview.checking.is_some() || preview.selected.is_empty() {
            return;
        }
        let mut rows: Vec<_> = preview
            .selected
            .iter()
            .filter_map(|index| Some((*index, preview.keys.address(*index).ok()?)))
            .collect();
        rows.shuffle(&mut rand::rng());
        let chain_id = chain.chain_id;
        for (index, _) in &rows {
            preview
                .results
                .insert((chain_id, *index), BalanceCheck::Queued);
        }
        preview.checking = Some(rows.iter().map(|(index, _)| *index).collect());
        state.check_run = state.check_run.wrapping_add(1);
        let run = state.check_run;
        let lease = state.lease.clone();
        let generation = self.active_wallet_generation;
        let http = self.http.clone();
        let runtime = self.runtime.clone();
        cx.spawn(async move |this, cx| {
            for (position, (index, address)) in rows.into_iter().enumerate() {
                if position > 0 {
                    cx.background_executor().timer(isolated_read_jitter()).await;
                }
                let key = (chain_id, index);
                let started = this.update(cx, |root, cx| {
                    root.record_derive_balance_check(
                        run,
                        generation,
                        &lease,
                        key,
                        BalanceCheck::Loading,
                        cx,
                    )
                });
                if !started.unwrap_or(false) {
                    return;
                }
                let (http, chain) = (http.clone(), chain.clone());
                let result =
                    runtime
                        .spawn(async move {
                            wallet_ops::check_native_balance(&http, &chain, address).await
                        })
                        .await;
                let check = match result {
                    Ok(Ok((balance, _))) => BalanceCheck::Done(balance),
                    _ => BalanceCheck::Failed,
                };
                let recorded = this.update(cx, |root, cx| {
                    root.record_derive_balance_check(run, generation, &lease, key, check, cx)
                });
                if !recorded.unwrap_or(false) {
                    return;
                }
            }
            let _ = this.update(cx, |root, cx| {
                let state = &mut root.public_form.derive_preview;
                if state.check_run == run
                    && let Some(preview) = state.preview.as_mut()
                {
                    preview.checking = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Records one address's check while its run is current, and reports whether it is.
    fn record_derive_balance_check(
        &mut self,
        run: u64,
        generation: u64,
        lease: &Weak<Cell<bool>>,
        key: (u64, u32),
        check: BalanceCheck,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let current = self.active_wallet_generation == generation
            && lease.upgrade().is_some_and(|open| open.get())
            && self.public_form.derive_preview.check_run == run;
        let Some(preview) = self
            .public_form
            .derive_preview
            .preview
            .as_mut()
            .filter(|_| current)
        else {
            return false;
        };
        preview.results.insert(key, check);
        cx.notify();
        true
    }

    pub(super) fn go_to_derive_preview_index(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let index = self
            .public_form
            .derive_preview
            .go_to_input
            .read(cx)
            .value()
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|index| *index < INDEX_LIMIT);
        match index {
            Some(index) => self.show_derive_preview_page(index, window, cx),
            None => {
                if let Some(preview) = self.public_form.derive_preview.preview.as_mut() {
                    preview.go_to_error = Some(Arc::from(format!(
                        "Enter an index from 0 to {}.",
                        INDEX_LIMIT - 1
                    )));
                    cx.notify();
                }
            }
        }
    }

    /// Adds the selected indexes. Runs on the UI thread so adds stay strictly one at a time.
    pub(super) fn add_selected_derived_public_accounts(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let (Some(store), Some(view_session)) =
            (self.vault_store.clone(), self.view_session.clone())
        else {
            return;
        };
        let Some(preview) = self.public_form.derive_preview.preview.as_mut() else {
            return;
        };
        if preview.selected.is_empty() {
            return;
        }
        let indexes: Vec<u32> = preview.selected.iter().copied().collect();
        match store.add_previewed_public_accounts(&view_session, &preview.keys, &indexes) {
            Ok(result) => {
                preview.selected.clear();
                let added: Vec<String> = result
                    .added
                    .iter()
                    .map(|account| account.public_account_uuid.clone())
                    .collect();
                preview.last_add = Some(result);
                self.public_form.error = None;
                if let Some(first) = added.first() {
                    self.public_form.selected_account_uuid = Some(Arc::from(first.as_str()));
                }
                self.reload_public_accounts(window, cx);
                if !added.is_empty() {
                    self.schedule_account_public_balance_refresh(
                        self.selected_chain,
                        added,
                        None,
                        cx,
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    "add previewed public accounts failed"
                );
                self.public_form.error = Some(vault_error_message(&error));
            }
        }
        cx.notify();
    }

    pub(super) fn render_derive_preview_content(
        &self,
        root: Entity<Self>,
        content_width: Pixels,
        device_auth_lease: Weak<Cell<bool>>,
    ) -> gpui::Div {
        let error = self.public_form.error.as_ref().map(|message| {
            Alert::error("wallet-public-add-derived-error", message.to_string()).small()
        });
        let state = &self.public_form.derive_preview;
        let Some(preview) = state.preview.as_ref() else {
            let busy = state.authorizing || self.device_auth_in_progress;
            return div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .debug_selector(|| "public-derive-authorize".into())
                .child(app_muted_text(
                    "Enter the vault password to show the Public addresses derived from the selected Private wallet. Nothing is saved until you add accounts.",
                ))
                .child(masked_input_with_device_auth(
                    &self.public_form.add_password_input,
                    busy,
                    self.public_account_device_auth_buttons(
                        root.clone(),
                        PublicAccountDialogKind::Derive,
                        "wallet-public-add-derived-touch-id",
                        state.authorizing,
                        device_auth_lease,
                    ),
                ))
                .children(error)
                .child(
                    app_button(
                        "wallet-public-add-derived-submit",
                        if state.authorizing {
                            "Authorizing..."
                        } else {
                            "Show addresses"
                        },
                    )
                    .primary()
                    .small()
                    .loading(state.authorizing)
                    .disabled(busy)
                    .on_click(move |_event, window, cx| {
                        root.update(cx, |root, cx| {
                            root.add_public_derived_account_from_input(window, cx);
                        });
                    }),
                );
        };

        let statuses = self.derived_index_statuses();
        let selected = preview.selected.len();
        let at_cap = selected >= DerivedPublicAccountPreview::MAX_ADD_INDEXES;
        let can_add = selected > 0;
        let start = preview.page_start;
        let rows = preview.keys.page(start).unwrap_or_default();
        let end = rows.last().map_or(start, |(index, _)| *index);
        let mut page_available = rows
            .iter()
            .map(|(index, _)| *index)
            .filter(|index| !statuses.contains_key(index))
            .peekable();
        let page_has_available = page_available.peek().is_some();
        let page_selected =
            page_has_available && page_available.all(|index| preview.selected.contains(&index));
        let add = {
            let root = root.clone();
            move |window: &mut Window, cx: &mut App| {
                root.update(cx, |root, cx| {
                    root.add_selected_derived_public_accounts(window, cx);
                });
            }
        };
        let add_on_enter = add.clone();
        let go_to = {
            let root = root.clone();
            move |window: &mut Window, cx: &mut App| {
                root.update(cx, |root, cx| root.go_to_derive_preview_index(window, cx));
            }
        };
        let go_to_on_enter = go_to.clone();
        let chain = self.effective_chain_configs.get(preview.chain_id);
        let check = {
            let root = root.clone();
            move |window: &mut Window, cx: &mut App| {
                root.update(cx, |root, cx| root.start_derive_balance_check(window, cx));
            }
        };
        let review_button = |id: &'static str, label: &'static str, open: bool| {
            let root = root.clone();
            app_button(id, label)
                .small()
                .on_click(move |_event, window, cx| {
                    root.update(cx, |root, cx| {
                        root.review_derive_balance_check(open, window, cx);
                    });
                })
        };
        if let (true, Some(chain)) = (preview.reviewing_check, chain) {
            let (summary, warning) =
                balance_check_summary(chain, self.http.network_mode(), &preview.selected);
            let check_on_enter = check.clone();
            return div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .debug_selector(|| "public-derive-check-summary".into())
                .track_focus(&state.focus)
                .on_action(move |_: &Confirm, window, cx| {
                    cx.stop_propagation();
                    check_on_enter(window, cx);
                })
                .child(app_muted_text(
                    "Checks the native balance of each selected address. Nothing is added or saved.",
                ))
                .child(
                    DescriptionList::horizontal()
                        .small()
                        .bordered(false)
                        .columns(1)
                        .label_width(rems(8.))
                        .children(summary.into_iter().map(|(label, value)| {
                            DescriptionItem::new(label).value(app_text(value).into_any_element())
                        })),
                )
                .child(Alert::warning("wallet-public-derive-check-warning", warning).small())
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(review_button(
                            "wallet-public-derive-check-cancel",
                            "Cancel",
                            false,
                        ))
                        .child(
                            app_button("wallet-public-derive-check-confirm", "Check balances")
                                .primary()
                                .small()
                                .on_click(move |_event, window, cx| check(window, cx)),
                        ),
                );
        }
        let progress = preview.checking.as_ref().map(|indexes| {
            let done = indexes
                .iter()
                .filter(|index| {
                    matches!(
                        preview.results.get(&(preview.chain_id, **index)),
                        Some(BalanceCheck::Done(_) | BalanceCheck::Failed)
                    )
                })
                .count();
            app_muted_text(format!("Checked {done} of {}", indexes.len()))
        });
        let check_action = if preview.checking.is_some() {
            let root = root.clone();
            app_button("wallet-public-derive-check-stop", "Stop")
                .small()
                .on_click(move |_event, window, cx| {
                    root.update(cx, |root, cx| root.stop_derive_balance_check(window, cx));
                })
        } else {
            review_button("wallet-public-derive-check", "Check native balances", true)
                .disabled(chain.is_none() || !can_add)
        };
        let page_button =
            |id: &'static str, icon: IconName, label: &'static str, target: u32, disabled: bool| {
                let root = root.clone();
                super::components::public_account_icon_button(id, icon, label)
                    .disabled(disabled)
                    .on_click(move |_event, window, cx| {
                        root.update(cx, |root, cx| {
                            root.show_derive_preview_page(target, window, cx);
                        });
                    })
            };

        div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .debug_selector(|| "public-derive-preview".into())
            .track_focus(&state.focus)
            .on_action(move |_: &Confirm, window, cx| {
                cx.stop_propagation();
                if can_add {
                    add_on_enter(window, cx);
                }
            })
            .child(app_muted_text(
                "Select the addresses to add as Public accounts.",
            ))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(page_button(
                        "wallet-public-derive-previous",
                        IconName::ChevronLeft,
                        "Previous page",
                        start.saturating_sub(PAGE_SIZE),
                        start == 0,
                    ))
                    .child(page_button(
                        "wallet-public-derive-next",
                        IconName::ChevronRight,
                        "Next page",
                        start.saturating_add(PAGE_SIZE),
                        start.saturating_add(PAGE_SIZE) >= INDEX_LIMIT,
                    ))
                    .child(app_muted_text(format!("{start}–{end}")))
                    .child(div().flex_1())
                    // Wraps as one unit in a very narrow window instead of shrinking.
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_2()
                            .child(
                                input_enter_scope(true, go_to_on_enter)
                                    .w(px(112.0))
                                    .child(app_input(&state.go_to_input).small()),
                            )
                            .child(
                                app_button("wallet-public-derive-go-to", "Go")
                                    .small()
                                    .on_click(move |_event, window, cx| go_to(window, cx)),
                            ),
                    ),
            )
            .children(preview.go_to_error.as_ref().map(|message| {
                Alert::error("wallet-public-derive-go-to-error", message.to_string()).small()
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        chain_select(&preview.chain_select)
                            .small()
                            .w(px(180.0))
                            .menu_width(px(240.0)),
                    )
                    .child(div().flex_1())
                    .children(progress)
                    .child(check_action),
            )
            .child(div().flex().child({
                let root = root.clone();
                Checkbox::new("wallet-public-derive-select-page")
                    .label("Select all on page")
                    .checked(page_selected)
                    .small()
                    .disabled(!page_has_available || (at_cap && !page_selected))
                    .on_click(move |checked, window, cx| {
                        let checked = *checked;
                        root.update(cx, |root, cx| {
                            root.select_derive_preview_page(checked, window, cx);
                        });
                    })
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(rows.into_iter().map(|(index, address)| {
                        let checked = preview.selected.contains(&index);
                        // Clicking the index or address follows the checkbox's rules.
                        let selectable = (checked || !at_cap) && !statuses.contains_key(&index);
                        let status = match statuses.get(&index) {
                            Some(PublicAccountStatus::Active) => {
                                app_muted_text("Added").into_any_element()
                            }
                            Some(PublicAccountStatus::Inactive) => {
                                app_muted_text("Inactive").into_any_element()
                            }
                            None => {
                                let root = root.clone();
                                Checkbox::new(("wallet-public-derive-select", index))
                                    .checked(checked)
                                    .small()
                                    .disabled(at_cap && !checked)
                                    .on_click(move |checked, window, cx| {
                                        let checked = *checked;
                                        root.update(cx, |root, cx| {
                                            root.toggle_derive_preview_index(
                                                index, checked, window, cx,
                                            );
                                        });
                                    })
                                    .into_any_element()
                            }
                        };
                        let balance = preview.results.get(&(preview.chain_id, index)).map(
                            |check| match check {
                                BalanceCheck::Queued => app_muted_text("Queued"),
                                BalanceCheck::Loading => app_muted_text("Checking…"),
                                BalanceCheck::Done(amount) => app_text(chain.map_or_else(
                                    || amount.to_string(),
                                    |chain| chain.native_currency.format_amount(*amount),
                                )),
                                BalanceCheck::Failed => app_muted_text("Failed"),
                            },
                        );
                        let copied = SharedString::from(super::public_address_qr_payload(address));
                        let toggle_root = root.clone();
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .id(("wallet-public-derive-row", index))
                                    .debug_selector(move || format!("public-derive-row-{index}"))
                                    .flex()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .items_center()
                                    .gap_3()
                                    .when(selectable, |area| {
                                        area.cursor_pointer().on_click(move |_event, window, cx| {
                                            toggle_root.update(cx, |root, cx| {
                                                root.toggle_derive_preview_index(
                                                    index, !checked, window, cx,
                                                );
                                            });
                                        })
                                    })
                                    .child(
                                        app_muted_text(index.to_string()).w(px(88.0)).flex_none(),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .items_center()
                                            .gap_1()
                                            .child(
                                                app_text(short_address(&address))
                                                    .font_family(ui::theme::APP_MONO_FONT_FAMILY)
                                                    .flex_none(),
                                            )
                                            .child(
                                                super::components::public_account_icon_button(
                                                    ("wallet-public-derive-copy", index),
                                                    IconName::Copy,
                                                    "Copy address",
                                                )
                                                .on_click(move |_event, window, cx| {
                                                    cx.stop_propagation();
                                                    ui::clipboard::copy_to_clipboard_with_toast(
                                                        copied.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                }),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .w(px(96.0))
                                    .flex_none()
                                    .flex()
                                    .justify_end()
                                    .children(balance.map(|text| text.min_w(px(0.0)).truncate())),
                            )
                            .child(div().w(px(64.0)).flex().justify_end().child(status))
                    })),
            )
            .children(preview.last_add.as_ref().map(|result| {
                let message = add_result_message(result);
                if result.skipped.is_empty() {
                    Alert::success("wallet-public-derive-added", message).small()
                } else {
                    Alert::warning("wallet-public-derive-added", message).small()
                }
            }))
            .children(error)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        app_muted_text(format!(
                            "{selected} of {} selected",
                            DerivedPublicAccountPreview::MAX_ADD_INDEXES
                        ))
                        .flex_1(),
                    )
                    .child(
                        app_button(
                            "wallet-public-derive-add",
                            format!("Add {}", count_label(selected, "account")),
                        )
                        .primary()
                        .small()
                        .disabled(!can_add)
                        .on_click(move |_event, window, cx| add(window, cx)),
                    ),
            )
    }
}
