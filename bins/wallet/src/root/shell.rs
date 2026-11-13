use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use broadcaster_monitor::{EventRx, EventTx, Shared};
use gpui::FontWeight;
use gpui::{
    App, AppContext, Bounds, Context, Entity, Focusable, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Point, Render, SharedString, StatefulInteractiveElement, Styled,
    Window, WindowBounds, WindowOptions, div, img, prelude::FluentBuilder as _, px, rgb, size,
};
use gpui_component::{
    ActiveTheme, Icon, Root, Sizable, TitleBar, WindowExt,
    badge::Badge,
    kbd::Kbd,
    notification::Notification,
    tab::{Tab, TabBar},
    tooltip::Tooltip,
};
use tokio::runtime::Handle;
use ui::controls::app_button;
use ui::icons;
use ui::logs::LogStore;
use ui::theme::{self, APP_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::WalletPpoiWorkflowStatus;

use crate::assets::LOGO_ICON_PATH;

use super::actions::{
    NextWalletTab, OpenChainSelector, OpenWalletSelector, PreviousWalletTab,
    WALLET_WORKSPACE_KEY_CONTEXT, register_wallet_shortcut_root,
};
use super::chain_load::{
    PresenceStatus, SyncStatusContext, WalletStatusCounts, balances_presence_status,
    ppoi_presence_status, ppoi_validation_toast_scope_is_current, ready_status_center,
    sync_status_center,
};
use super::private_assets::{
    format_private_asset_rows_from_snapshot, should_show_pending_poi_amount,
};
use super::settings::{data_folder_path, data_folder_row};
use super::submission_route::status_presence_text;
use super::utxo::{
    blocked_shield_rescue_display_rows, recoverable_poi_candidate_count, should_focus_utxo_table,
};
use super::{
    Activity, ChainUtxoState, SIDEBAR_AUTO_COLLAPSE_WIDTH, VaultState, WalletRoot,
    WalletStartupRoot, app_status_tag, chain_load_overrides, rgb_with_alpha,
    should_apply_background_focus,
};

mod balances_status;
mod hero;
mod poi_retry;
mod ppoi_status;

use balances_status::render_balances_status_hover_target;
#[cfg(test)]
pub(super) use balances_status::{
    balance_sync_data_source, balance_sync_issue_detail, balance_sync_source_label,
};
pub(super) use hero::{
    COPY_URL_TOOLTIP, LINK_COPIED_MESSAGE, TELEGRAM_URL, render_wallet_hero_screen,
    wallet_build_label,
};
pub(super) use poi_retry::PoiArtifactCacheRetryAttempts;
#[cfg(test)]
pub(super) use poi_retry::ppoi_retry_completion_is_current;
use ppoi_status::{ppoi_attention_badge_color, render_ppoi_status_hover_target};
#[cfg(test)]
pub(super) use ppoi_status::{
    ppoi_chunk_progress_label, ppoi_hover_detail, ppoi_hover_heading, ppoi_replay_progress_label,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum WalletTab {
    #[default]
    Private,
    Public,
    Activity,
    UTXOs,
}

impl WalletTab {
    pub(super) const ALL: [Self; 4] = [Self::Private, Self::Public, Self::Activity, Self::UTXOs];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Private => "Private",
            Self::Public => "Public",
            Self::Activity => "Activity",
            Self::UTXOs => "UTXOs",
        }
    }

    pub(super) const fn icon_path(self) -> &'static str {
        match self {
            Self::Private => icons::shield_keyhole_icon_path(),
            Self::Public => icons::eye_icon_path(),
            Self::Activity => icons::activity_icon_path(),
            Self::UTXOs => crate::assets::LIST_ICON_PATH,
        }
    }

    pub(super) const fn shows_utxos(self) -> bool {
        matches!(self, Self::UTXOs)
    }

    /// Private and UTXOs show private notes, which only a chain with Railgun has. Public and
    /// Activity work on every network.
    pub(super) const fn needs_railgun(self) -> bool {
        matches!(self, Self::Private | Self::UTXOs)
    }
}

#[derive(Clone)]
pub(crate) struct WalletAppOptions {
    pub(super) db_path: PathBuf,
}

impl TryFrom<crate::cli::Options> for WalletAppOptions {
    type Error = eyre::Report;

    fn try_from(value: crate::cli::Options) -> Result<Self, Self::Error> {
        Ok(Self {
            db_path: value.db_path.unwrap_or_else(crate::cli::default_db_path),
        })
    }
}

pub(crate) fn open_wallet_window(
    app: &mut App,
    options: WalletAppOptions,
    runtime: Handle,
    monitor: Shared,
    event_tx: EventTx,
    event_rx: EventRx,
    chain_ids: &[u64],
    logs: LogStore,
) {
    wallet_ops::vault::enable_best_effort_runtime_hardening();
    let chain_ids = chain_ids.to_vec();
    let window_options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(1_360.0), px(860.0)),
        })),
        app_id: {
            #[cfg(target_os = "linux")]
            {
                Some(
                    std::env::var("FLATPAK_ID")
                        .ok()
                        .filter(|app_id| !app_id.is_empty())
                        .unwrap_or_else(|| "app.railoxide.wallet".to_owned()),
                )
            }
            #[cfg(not(target_os = "linux"))]
            {
                None
            }
        },
        titlebar: Some(wallet_titlebar_options()),
        window_decorations: Some(gpui::WindowDecorations::Client),
        ..Default::default()
    };

    if let Err(error) = app.open_window(window_options, |window, cx| {
        let root = cx.new(|cx| {
            WalletStartupRoot::new(
                options, runtime, monitor, event_tx, event_rx, &chain_ids, logs, window, cx,
            )
        });
        register_wallet_shortcut_root(window, &root, cx);
        cx.new(|cx| Root::new(root, window, cx))
    }) {
        tracing::error!(%error, "failed to open wallet window");
    }
}

impl WalletRoot {
    /// The tab a wallet opens on: Private, or Public where the selected chain has no Railgun.
    pub(super) fn default_wallet_tab(&self) -> WalletTab {
        if self.selected_chain_has_railgun() {
            WalletTab::default()
        } else {
            WalletTab::Public
        }
    }

    pub(super) fn select_wallet_tab(&mut self, tab: WalletTab, cx: &mut Context<'_, Self>) {
        if tab.needs_railgun() && !self.selected_chain_has_railgun() {
            return;
        }
        if self.active_wallet_tab == tab {
            return;
        }
        self.active_wallet_tab = tab;
        self.focus_utxo_table_on_render = should_focus_utxo_table(
            self.active_activity,
            self.active_wallet_tab,
            self.chain_states.get(&self.last_evm_chain),
        );
        if tab == WalletTab::Public {
            self.focus_public_account_search_on_render = true;
            self.schedule_automatic_public_balance_refresh(cx);
        }
        if tab == WalletTab::Activity {
            // Reading again picks up address-book labels and accounts changed since.
            self.load_wallet_activity(cx);
            self.mark_wallet_activity_seen(cx);
            self.wallet_activity.request_list_focus();
        }
        cx.notify();
    }

    /// Selects the next (or previous) wallet tab with wrap-around, skipping tabs
    /// the selected chain cannot show.
    pub(super) fn cycle_wallet_tab(&mut self, forward: bool, cx: &mut Context<'_, Self>) {
        let tabs = WalletTab::ALL;
        let count = tabs.len();
        let current = tabs
            .iter()
            .position(|tab| *tab == self.active_wallet_tab)
            .unwrap_or(0);
        let has_railgun = self.selected_chain_has_railgun();
        let next = (1..count)
            .map(|step| {
                if forward {
                    tabs[(current + step) % count]
                } else {
                    tabs[(current + count - step) % count]
                }
            })
            .find(|tab| !tab.needs_railgun() || has_railgun);
        if let Some(tab) = next {
            self.select_wallet_tab(tab, cx);
        }
    }

    /// Keeps wallet shortcuts reachable when the focused element disappears, such as the
    /// vault password input after unlock or the public search input after a tab switch.
    pub(super) fn focus_wallet_view_after_focus_loss(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if matches!(self.vault_state, VaultState::ViewUnlocked)
            && self.active_activity == Activity::Wallet
            && should_apply_background_focus(window.has_active_dialog(cx))
        {
            self.wallet_focus.focus(window, cx);
        }
    }

    pub(super) fn focus_public_account_search_if_requested(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.focus_public_account_search_on_render
            || self.active_activity != Activity::Wallet
            || self.active_wallet_tab != WalletTab::Public
        {
            return;
        }

        self.public_form
            .search_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        self.focus_public_account_search_on_render = false;
    }
}

impl Render for WalletRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if self.private_pending_status_dialog_open && !window.has_active_dialog(cx) {
            self.private_pending_status_dialog_open = false;
        }
        let pending_ppoi_validation_toast = self.pending_ppoi_validation_toast.take();
        if pending_ppoi_validation_toast.is_some_and(|(wallet_id, wallet_generation)| {
            ppoi_validation_toast_scope_is_current(
                self.selected_wallet_id.as_deref(),
                self.active_wallet_generation,
                wallet_id.as_ref(),
                wallet_generation,
            )
        }) {
            window.push_notification(
                Notification::success("Outgoing transaction proofs recovered."),
                cx,
            );
        }
        if std::mem::take(&mut self.pending_activity_journal_notice) {
            window.push_notification(
                Notification::warning(
                    "Couldn't update Activity for a recent transaction. The transaction itself \
                     wasn't affected.",
                ),
                cx,
            );
        }
        self.apply_public_broadcaster_error_amount_adjustments(window, cx);
        self.sync_platform_attention_for_window(window);
        self.ensure_prover_cache_build_monitor(cx);
        self.ensure_stealth_accounts(window, cx);
        self.ensure_private_swaps(window, cx);
        if should_apply_background_focus(window.has_active_dialog(cx)) {
            self.focus_vault_input_if_requested(window, cx);
            self.prompt_device_auth_on_unlock_screen_if_requested(window, cx);
            self.focus_utxo_table_if_requested(window, cx);
            self.focus_public_account_search_if_requested(window, cx);
            self.focus_wallet_activity_list_if_requested(window, cx);
        }

        let root = cx.entity();
        if !matches!(self.vault_state, VaultState::ViewUnlocked) {
            // Debug UI fixture: an `update-check:` mode opens its dialog over the first frame.
            #[cfg(debug_assertions)]
            self.open_ui_fixture_update_check(window, cx);
            return self.render_locked_vault_screen(root, window);
        }
        self.open_next_walletconnect_request_dialog_if_idle(window, cx);
        let sidebar_is_narrow = window.viewport_size().width < SIDEBAR_AUTO_COLLAPSE_WIDTH;
        if !sidebar_is_narrow {
            self.sidebar_narrow_expanded = false;
        }
        let sidebar_collapsed = if sidebar_is_narrow {
            !self.sidebar_narrow_expanded
        } else {
            self.sidebar_manually_collapsed
        };
        let dialog_open = window.has_active_dialog(cx);

        div()
            .when(self.settings_editor.is_some(), |this| {
                this.on_action(cx.listener(
                    |this, action: &ui::chain_select::EditChain, window, cx| {
                        this.open_chain_editor(action.chain_id, window, cx);
                    },
                ))
            })
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .text_color(rgb(theme::TEXT))
            .font_family(APP_FONT_FAMILY)
            .font_weight(FontWeight::LIGHT)
            .text_size(APP_TEXT_SIZE)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .flex()
                    .child(self.render_sidebar(root.clone(), sidebar_collapsed, sidebar_is_narrow))
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex_1()
                                    .min_h(px(0.0))
                                    .child(self.render_workspace(&root, window, cx)),
                            ),
                    ),
            )
            .child(self.render_bottom_block(&root, dialog_open, window, cx))
    }
}

fn wallet_titlebar_options() -> gpui::TitlebarOptions {
    let mut options = TitleBar::title_bar_options();
    options.title = Some(SharedString::from("RailOxide"));
    options
}

pub(super) fn render_wallet_window_frame(
    content: gpui::AnyElement,
    window: &Window,
    titlebar_color: u32,
) -> gpui::Div {
    div()
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .text_color(rgb(theme::TEXT))
        .font_family(APP_FONT_FAMILY)
        .font_weight(FontWeight::LIGHT)
        .text_size(APP_TEXT_SIZE)
        .when(should_render_wallet_title_bar(window), |this| {
            this.child(render_wallet_title_bar(titlebar_color))
        })
        .child(div().flex_1().min_w(px(0.0)).min_h(px(0.0)).child(content))
}

fn should_render_wallet_title_bar(window: &Window) -> bool {
    !cfg!(any(target_os = "linux", target_os = "freebsd"))
        || matches!(
            window.window_decorations(),
            gpui::Decorations::Client { .. }
        )
}

fn render_wallet_title_bar(titlebar_color: u32) -> TitleBar {
    TitleBar::new()
        .bg(rgb(titlebar_color))
        .border_color(rgb(titlebar_color))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w(px(0.0))
                .child(img(LOGO_ICON_PATH).size(px(16.0)))
                .child(
                    div()
                        .text_color(rgb(theme::TEXT))
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child("RailOxide"),
                ),
        )
}

impl WalletRoot {
    pub(super) fn render_workspace(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let active_content = self.render_active_content(root, window, cx);
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .child(active_content)
    }

    fn render_active_content(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::AnyElement {
        match self.active_activity {
            Activity::Wallet => self.render_wallet_view(root, window, cx).into_any_element(),
            Activity::Broadcaster => self.render_broadcaster_view(root).into_any_element(),
            Activity::AddressBook => self.render_address_book_view(root),
            Activity::Proposals => self
                .render_governance_workspace(root, window, cx)
                .into_any_element(),
            Activity::Settings => self.render_settings_view(root).into_any_element(),
        }
    }

    fn render_settings_view(&self, root: &Entity<Self>) -> impl IntoElement {
        let content = if let Some(editor) = self.settings_editor.as_ref() {
            div().size_full().child(editor.clone()).into_any_element()
        } else {
            let open_root = root.clone();
            div()
                .p(px(24.0))
                .flex()
                .flex_col()
                .gap_3()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(
                    self.settings_error.as_ref().map_or_else(
                        || "Settings are unavailable".to_string(),
                        ToString::to_string,
                    ),
                ))
                .child(data_folder_row(
                    &data_folder_path(&self.options.db_path),
                    None,
                    true,
                    self.data_folder_open_error.clone(),
                    move |_window, cx| {
                        open_root.update(cx, Self::open_settings_data_folder);
                    },
                ))
                .into_any_element()
        };
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .bg(rgb(theme::SURFACE))
            .p(px(16.0))
            .flex()
            .flex_col()
            .gap_3()
            .child(div().flex_1().min_h(px(0.0)).child(content))
    }

    pub(super) fn render_wallet_view(
        &self,
        root: &Entity<Self>,
        window: &Window,
        cx: &gpui::App,
    ) -> impl IntoElement {
        let wallet_focus = self.wallet_focus.clone();
        div()
            .key_context(WALLET_WORKSPACE_KEY_CONTEXT)
            .track_focus(&self.wallet_focus)
            // Clicking empty wallet space claims focus only when it is outside the view,
            // so an inner input keeps focus.
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                if wallet_focus.contains_focused(window, cx) {
                    window.prevent_default();
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &NextWalletTab, _, cx| {
                    root.update(cx, |root, cx| root.cycle_wallet_tab(true, cx));
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &PreviousWalletTab, _, cx| {
                    root.update(cx, |root, cx| root.cycle_wallet_tab(false, cx));
                }
            })
            // SelectState has no public open method, so focus the trigger and send it the
            // key that opens it. The dispatch is deferred and targets the trigger focused here.
            .on_action({
                let root = root.clone();
                move |_: &OpenWalletSelector, window, cx| {
                    let select = root.read(cx).wallet_select.clone();
                    select.update(cx, |select, cx| select.focus(window, cx));
                    window.dispatch_action(Box::new(gpui_kit::base::actions::SelectDown), cx);
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &OpenChainSelector, window, cx| {
                    let select = root.read(cx).chain_select.clone();
                    select.update(cx, |select, cx| select.focus(window, cx));
                    window.dispatch_action(Box::new(gpui_kit::base::actions::SelectDown), cx);
                }
            })
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .child(self.render_wallet_header(root))
            .child(self.render_wallet_tabs(root, window, cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .p(px(12.0))
                    .child(self.render_wallet_content(root, window, cx)),
            )
    }

    /// Status bar center (sync progress) and right chips for the selected wallet on its
    /// selected chain, on every workspace.
    pub(super) fn render_wallet_status(
        &self,
        root: &Entity<Self>,
        dialog_open: bool,
    ) -> (Option<gpui::Div>, Vec<gpui::AnyElement>) {
        let Some(state) = self
            .selected_evm_chain()
            .and_then(|chain_id| self.chain_states.get(&chain_id))
            .filter(|state| state.renders_table())
        else {
            return (None, Vec::new());
        };
        let counts = self.wallet_status_counts(
            state.snapshot().map(AsRef::as_ref),
            state.ppoi_workflow_status(),
        );
        let center = match state {
            ChainUtxoState::Loading { .. } => Some(sync_status_center(
                SyncStatusContext::Loading,
                state.progress(),
            )),
            ChainUtxoState::Syncing { .. } => Some(sync_status_center(
                SyncStatusContext::Syncing,
                state.progress(),
            )),
            ChainUtxoState::Ready { .. } => ready_status_center(counts),
            ChainUtxoState::Idle | ChainUtxoState::Error { .. } => None,
        };
        (
            center,
            self.render_wallet_status_chips(root, state, counts, dialog_open),
        )
    }

    fn wallet_status_counts(
        &self,
        snapshot: Option<&wallet_ops::ListUtxosOutput>,
        ppoi_workflow_status: WalletPpoiWorkflowStatus,
    ) -> WalletStatusCounts {
        let Some(snapshot) = snapshot else {
            return WalletStatusCounts {
                ppoi_workflow_status,
                ..WalletStatusCounts::default()
            };
        };
        let assets = format_private_asset_rows_from_snapshot(
            snapshot,
            Some(&self.effective_token_registry),
            Some(&self.public_broadcaster_anchor_cache),
        );
        WalletStatusCounts {
            pending_incoming_outputs: snapshot.utxos.iter().filter(|row| row.pending_new).count(),
            pending_outgoing_outputs: snapshot
                .utxos
                .iter()
                .filter(|row| row.pending_spent || row.local_pending_spent)
                .count(),
            pending_poi_assets: assets
                .iter()
                .filter(|asset| should_show_pending_poi_amount(asset.pending_poi_total))
                .count(),
            recoverable_poi_outputs: recoverable_poi_candidate_count(snapshot),
            blocked_shield_outputs: blocked_shield_rescue_display_rows(
                snapshot,
                &self.blocked_shield_rescue_rows,
                &self.blocked_shield_refunds_in_flight,
            )
            .len(),
            ppoi_workflow_status,
        }
    }

    fn ppoi_status_for_state(
        &self,
        state: &ChainUtxoState,
        counts: WalletStatusCounts,
    ) -> PresenceStatus {
        ppoi_presence_status(
            state.poi_refreshing(),
            state.poi_refresh_session().is_some(),
            self.poi_read_source.is_indexed_artifacts(),
            self.selected_chain_poi_artifact_progress(),
            counts,
        )
    }

    fn balances_status_for_state(&self, state: &ChainUtxoState) -> PresenceStatus {
        let Some(block_time) = self
            .effective_chain_configs
            .get(self.last_evm_chain)
            .and_then(|chain| chain.block_time)
        else {
            return PresenceStatus::Unknown;
        };
        balances_presence_status(
            state.is_syncing(),
            matches!(state, ChainUtxoState::Ready { .. }),
            state.sync_tip(),
            block_time,
            now_epoch_secs(),
        )
    }

    fn render_wallet_status_chips(
        &self,
        root: &Entity<Self>,
        state: &ChainUtxoState,
        counts: WalletStatusCounts,
        dialog_open: bool,
    ) -> Vec<gpui::AnyElement> {
        let ppoi_status = self.ppoi_status_for_state(state, counts);
        let balances_status = self.balances_status_for_state(state);
        let mut chips = Vec::new();

        if counts.ppoi_status_count() > 0 {
            chips.push(Self::render_ppoi_status_indicator(
                root,
                ppoi_status,
                counts,
                "PPOI",
                dialog_open,
            ));
        } else {
            chips.push(
                render_ppoi_status_hover_target(root, "wallet-status-ppoi")
                    .child(status_presence_text("PPOI", ppoi_status))
                    .into_any_element(),
            );
        }
        chips.push(
            render_balances_status_hover_target(root, "wallet-status-balances")
                .child(status_presence_text("Balances", balances_status))
                .into_any_element(),
        );
        chips
    }

    fn render_ppoi_status_indicator(
        root: &Entity<Self>,
        status: PresenceStatus,
        counts: WalletStatusCounts,
        label: &'static str,
        dialog_open: bool,
    ) -> gpui::AnyElement {
        let details_root = root.clone();
        render_ppoi_status_hover_target(root, "wallet-status-ppoi-hover")
            // Above a dialog the chip keeps its hover details but opens nothing.
            .when(!dialog_open, |this| {
                this.cursor_pointer().on_click(move |_event, window, cx| {
                    details_root.update(cx, |root, cx| {
                        root.open_private_pending_status_dialog(window, cx);
                    });
                })
            })
            .child(
                Badge::new()
                    .count(counts.ppoi_status_count())
                    .color(rgb(ppoi_attention_badge_color(counts)))
                    .child(
                        status_presence_text(label, status)
                            .pr(px(12.0))
                            .into_any_element(),
                    ),
            )
            .into_any_element()
    }

    fn render_wallet_tabs(
        &self,
        root: &Entity<Self>,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        let selected_index = WalletTab::ALL
            .iter()
            .position(|tab| *tab == self.active_wallet_tab)
            .unwrap_or(0);
        let tab_root = root.clone();
        let pending_walletconnect_requests = self.walletconnect_pending_request_count();

        TabBar::new("wallet-tabs")
            .underline()
            .w_full()
            .flex_none()
            .px(px(14.0))
            .selected_index(selected_index)
            .when_some(self.render_wallet_tab_hint(window, cx), |tabs, hint| {
                tabs.suffix(hint)
            })
            .on_click(move |index, _window, cx| {
                let Some(tab) = WalletTab::ALL.get(*index).copied() else {
                    return;
                };
                tab_root.update(cx, |root, cx| {
                    root.select_wallet_tab(tab, cx);
                });
            })
            .children(WalletTab::ALL.into_iter().map(|tab| {
                let unavailable = tab.needs_railgun() && !self.selected_chain_has_railgun();
                let icon = Icon::empty()
                    .path(tab.icon_path())
                    .with_size(px(18.0))
                    .text_color(rgb(theme::TEXT));
                let unread = tab == WalletTab::Activity
                    && self.active_wallet_tab != WalletTab::Activity
                    && self.wallet_activity.unread();
                Tab::new()
                    .disabled(unavailable)
                    .when(unavailable, |tab| {
                        let reason = self.railgun_unavailable_reason();
                        tab.tooltip(move |window, cx| Tooltip::new(reason).build(window, cx))
                    })
                    .when(unread, |tab| {
                        tab.tooltip(|window, cx| {
                            Tooltip::new("Funds received since you last opened Activity")
                                .build(window, cx)
                        })
                    })
                    .min_w(px(92.0))
                    .label(tab.label())
                    .map(|tab| {
                        if unread {
                            tab.prefix(
                                div()
                                    .debug_selector(|| "wallet-tab-activity-unread".to_owned())
                                    .child(
                                        Badge::new().dot().color(rgb(theme::PRIMARY)).child(icon),
                                    ),
                            )
                        } else {
                            tab.prefix(icon)
                        }
                    })
                    .when(
                        tab == WalletTab::Public
                            && self.active_wallet_tab != WalletTab::Public
                            && pending_walletconnect_requests > 0,
                        |tab| {
                            tab.suffix(walletconnect_tab_attention_badge(
                                pending_walletconnect_requests,
                            ))
                        },
                    )
            }))
    }

    fn render_wallet_tab_hint(&self, window: &Window, cx: &App) -> Option<gpui::Div> {
        let key = Kbd::binding_for_action_in(&NextWalletTab, &self.wallet_focus, window)?;
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    key.outline()
                        .font_family(crate::assets::KEYCAP_FONT_FAMILY)
                        .font_weight(FontWeight::NORMAL)
                        .font_features(gpui::FontFeatures(Arc::new(vec![("case".into(), 1)]))),
                )
                .child(
                    div()
                        .text_size(gpui::rems(11.0 / 16.0))
                        .line_height(gpui::rems(14.0 / 16.0))
                        .text_color(cx.theme().muted_foreground)
                        .child("switch tab"),
                ),
        )
    }

    fn render_wallet_content(
        &self,
        root: &Entity<Self>,
        window: &Window,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        match self.active_wallet_tab {
            WalletTab::Private => self.render_private_assets_body(root, cx),
            WalletTab::Public => self.render_public_wallet_body(root, window, cx),
            WalletTab::Activity => self.render_wallet_activity_body(root, cx),
            WalletTab::UTXOs => self
                .render_utxo_body(root, window)
                .debug_selector(|| "wallet-utxos-body".to_owned())
                .into_any_element(),
        }
    }

    pub(super) fn render_chain_error_body(&self, root: &Entity<Self>, message: &str) -> gpui::Div {
        let can_retry =
            matches!(self.vault_state, VaultState::ViewUnlocked) && self.view_session.is_some();
        let retry_root = root.clone();

        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .max_w(px(520.0))
                    .text_color(rgb(theme::DANGER))
                    .text_align(gpui::TextAlign::Center)
                    .child(SharedString::from(message.to_owned())),
            )
            .when(can_retry, |this| {
                this.child(
                    app_button("wallet-chain-retry-sync", "Retry sync")
                        .outline()
                        .small()
                        .on_click(move |_event, _window, cx| {
                            retry_root.update(cx, |root, cx| {
                                if root.view_session.is_none() {
                                    return;
                                }
                                let chain_id = root.last_evm_chain;
                                let overrides = chain_load_overrides();
                                root.start_chain_load(chain_id, &overrides, true, cx);
                            });
                        }),
                )
            })
    }
}

fn render_status_hover_note_base(
    title: &str,
    detail: &str,
    color: u32,
    bg_alpha: f32,
) -> gpui::Div {
    div()
        .rounded_md()
        .border_1()
        .border_color(rgb(color))
        .bg(rgb_with_alpha(color, bg_alpha))
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(color))
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(12.0))
                .line_height(px(17.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(detail.to_string()),
        )
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn walletconnect_tab_attention_badge(count: usize) -> impl IntoElement {
    app_status_tag(attention_count_label(count), theme::WARNING)
}

fn attention_count_label(count: usize) -> String {
    if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    }
}
