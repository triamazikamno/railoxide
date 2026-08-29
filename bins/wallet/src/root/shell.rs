use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::hex;
use broadcaster_monitor::{EventRx, EventTx, Shared};
use gpui::ObjectFit;
use gpui::{
    App, AppContext, Bounds, Context, Entity, Focusable, InteractiveElement, IntoElement,
    ParentElement, Point, Render, SharedString, StatefulInteractiveElement, Styled,
    StyledImage as _, Window, WindowBounds, WindowOptions, div, img, prelude::FluentBuilder as _,
    px, rgb, size,
};
use gpui_component::{
    Disableable, Icon, IconName, Root, Sizable, TitleBar, WindowExt,
    badge::Badge,
    button::ButtonVariants,
    notification::Notification,
    progress::Progress as UiProgress,
    resizable::{resizable_panel, v_resizable},
    scroll::ScrollableElement,
    tab::{Tab, TabBar},
    tooltip::Tooltip,
};
use tokio::runtime::Handle;
use ui::clipboard::{clipboard_with_toast, copy_to_clipboard_with_custom_toast};
use ui::controls::{app_button, app_button_base};
use ui::icons;
use ui::logs::LogStore;
use ui::theme::{self, APP_FONT_FAMILY, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::{
    PoiArtifactCacheAttemptId, PoiArtifactCacheFailureKind, PoiArtifactCacheListProgress,
    PoiArtifactCachePhase, PoiArtifactCacheProgress, PublicScanSource, WalletNetworkMode,
    WalletPpoiWorkflowStatus, WalletSyncTip,
};

use crate::assets::{
    HEMATITE_HERO_PATH, HERO_WORDMARK_PATH, LOGO_ICON_PATH, RailgunSocialIcon, WARM_GLOW_PATH,
};

use super::actions::register_wallet_shortcut_root;
use super::chain_load::{
    BalanceSyncIssue, PresenceStatus, SyncStatusContext, SyncStatusLabels, WalletStatusCounts,
    balance_sync_issue, balances_presence_status, ppoi_presence_status,
    ppoi_validation_toast_scope_is_current, ready_status_bar, sync_status_bar, sync_status_labels,
};
use super::private_assets::{
    format_private_asset_rows_from_snapshot, should_show_pending_poi_amount,
};
use super::ui_helpers::format_binary_bytes;
use super::utxo::{
    blocked_shield_rescue_display_rows, ppoi_workflow_status_detail, ppoi_workflow_status_title,
    recoverable_poi_candidate_count, should_focus_utxo_table,
};
use super::{
    Activity, ChainUtxoState, HERO_CARD_MAX_WIDTH, HERO_MEDIUM_BREAKPOINT, HERO_STAGE_MAX_WIDTH,
    HERO_WIDE_BREAKPOINT, LOGS_DRAWER_HEIGHT, LOGS_DRAWER_MAX_HEIGHT, LOGS_DRAWER_MIN_HEIGHT,
    SIDEBAR_AUTO_COLLAPSE_WIDTH, VaultState, WalletRoot, WalletStartupRoot, app_status_tag,
    chain_load_overrides, count_label, rgb_with_alpha, should_apply_background_focus,
};

pub(super) const COPY_URL_TOOLTIP: &str = "Click to copy URL to clipboard";
pub(super) const LINK_COPIED_MESSAGE: &str = "Link copied to clipboard!";
pub(super) const RAILOXIDE_REPOSITORY_URL: &str = "https://github.com/triamazikamno/railoxide";
pub(super) const TELEGRAM_URL: &str = "https://t.me/railoxide";

#[derive(Default)]
pub(super) struct PoiArtifactCacheRetryAttempts {
    active: BTreeMap<u64, PoiArtifactCacheRetryAttempt>,
    next_request_token: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PoiArtifactCacheRetryAttempt {
    request_token: Option<u64>,
    attempt_id: Option<PoiArtifactCacheAttemptId>,
}

enum PoiArtifactCacheRetryTaskEvent {
    AdmissionFailed(String),
    Admitted(PoiArtifactCacheAttemptId),
    Finished {
        attempt_id: PoiArtifactCacheAttemptId,
        result: Result<(), String>,
    },
}

impl PoiArtifactCacheRetryAttempts {
    pub(super) fn begin(&mut self, chain_id: u64) -> Option<u64> {
        let request_token = self.next_request_token.checked_add(1)?;
        match self.active.entry(chain_id) {
            Entry::Occupied(_) => None,
            Entry::Vacant(entry) => {
                self.next_request_token = request_token;
                entry.insert(PoiArtifactCacheRetryAttempt {
                    request_token: Some(request_token),
                    attempt_id: None,
                });
                Some(request_token)
            }
        }
    }

    pub(super) fn bind(
        &mut self,
        chain_id: u64,
        request_token: u64,
        attempt_id: PoiArtifactCacheAttemptId,
    ) -> bool {
        let Some(attempt) = self.active.get_mut(&chain_id) else {
            return false;
        };
        if attempt.request_token != Some(request_token) || attempt.attempt_id.is_some() {
            return false;
        }
        attempt.request_token = None;
        attempt.attempt_id = Some(attempt_id);
        true
    }

    pub(super) fn cancel_pending(&mut self, chain_id: u64, request_token: u64) -> bool {
        match self.active.entry(chain_id) {
            Entry::Occupied(entry)
                if entry.get().request_token == Some(request_token)
                    && entry.get().attempt_id.is_none() =>
            {
                entry.remove();
                true
            }
            Entry::Occupied(_) | Entry::Vacant(_) => false,
        }
    }

    pub(super) fn finish(&mut self, chain_id: u64, attempt_id: PoiArtifactCacheAttemptId) -> bool {
        match self.active.entry(chain_id) {
            Entry::Occupied(entry) if entry.get().attempt_id == Some(attempt_id) => {
                entry.remove();
                true
            }
            Entry::Occupied(_) | Entry::Vacant(_) => false,
        }
    }

    pub(super) fn contains(&self, chain_id: u64) -> bool {
        self.active.contains_key(&chain_id)
    }

    pub(super) fn clear(&mut self) {
        self.active.clear();
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum WalletTab {
    #[default]
    Private,
    Public,
    Activity,
}

impl WalletTab {
    pub(super) const ALL: [Self; 3] = [Self::Private, Self::Public, Self::Activity];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Private => "Private",
            Self::Public => "Public",
            Self::Activity => "Activity",
        }
    }

    pub(super) const fn icon_path(self) -> &'static str {
        match self {
            Self::Private => icons::shield_check_icon_path(),
            Self::Public => icons::globe_icon_path(),
            Self::Activity => icons::activity_icon_path(),
        }
    }

    pub(super) const fn shows_utxos(self) -> bool {
        matches!(self, Self::Activity)
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
    fn select_wallet_tab(&mut self, tab: WalletTab, cx: &mut Context<'_, Self>) {
        if self.active_wallet_tab == tab {
            return;
        }
        self.active_wallet_tab = tab;
        self.focus_utxo_table_on_render = should_focus_utxo_table(
            self.active_activity,
            self.active_wallet_tab,
            self.chain_states.get(&self.selected_chain),
        );
        if tab == WalletTab::Public {
            self.focus_public_account_search_on_render = true;
            self.schedule_public_balance_refresh(cx);
        }
        cx.notify();
    }

    pub(super) fn focus_public_account_search_if_requested(
        &mut self,
        window: &mut Window,
        cx: &Context<'_, Self>,
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
            .focus(window);
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
        self.apply_public_broadcaster_error_amount_adjustments(window, cx);
        self.sync_walletconnect_attention_for_window(window);
        self.ensure_prover_cache_build_monitor(cx);
        if should_apply_background_focus(window.has_active_dialog(cx)) {
            self.focus_vault_input_if_requested(window, cx);
            self.focus_utxo_table_if_requested(window, cx);
            self.focus_public_account_search_if_requested(window, cx);
        }

        let root = cx.entity();
        if !matches!(self.vault_state, VaultState::ViewUnlocked) {
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

        div()
            .relative()
            .size_full()
            .flex()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .text_color(rgb(theme::TEXT))
            .font_family(APP_FONT_FAMILY)
            .text_size(APP_TEXT_SIZE)
            .child(self.render_sidebar(root.clone(), sidebar_collapsed, sidebar_is_narrow))
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .child(self.render_workspace(root, window, cx)),
            )
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
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("RailOxide"),
                ),
        )
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum WalletHeroLayout {
    Wide,
    Medium,
    Narrow,
}

fn wallet_hero_layout(window: &Window) -> WalletHeroLayout {
    let viewport = window.viewport_size();
    if viewport.width >= HERO_WIDE_BREAKPOINT && viewport.width >= viewport.height * 1.4 {
        WalletHeroLayout::Wide
    } else if viewport.width >= HERO_MEDIUM_BREAKPOINT {
        WalletHeroLayout::Medium
    } else {
        WalletHeroLayout::Narrow
    }
}

pub(super) fn render_wallet_hero_screen(window: &Window, card: gpui::AnyElement) -> gpui::Div {
    let viewport = window.viewport_size();
    let layout = wallet_hero_layout(window);
    let stage_width = (viewport.width - px(96.0))
        .max(px(0.0))
        .min(HERO_STAGE_MAX_WIDTH);
    let card_width = (viewport.width - px(48.0))
        .max(px(0.0))
        .min(HERO_CARD_MAX_WIDTH);
    let vertical_padding = match layout {
        WalletHeroLayout::Wide => px(32.0),
        WalletHeroLayout::Medium => px(40.0),
        WalletHeroLayout::Narrow => px(24.0),
    };
    let scroll_content_min_height = (viewport.height - vertical_padding * 2.0).max(px(0.0));

    let stage = if layout == WalletHeroLayout::Wide {
        div()
            .w(stage_width)
            .flex()
            .items_center()
            .gap_6()
            .child(
                render_wallet_brand_block(window, layout)
                    .w(px(560.0))
                    .flex_none(),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .justify_end()
                    .child(div().w(card_width).child(card)),
            )
    } else {
        div()
            .w(card_width)
            .flex()
            .flex_col()
            .items_center()
            .gap_6()
            .child(render_wallet_brand_block(window, layout).w_full())
            .child(div().w_full().child(card))
    };

    div()
        .relative()
        .size_full()
        .overflow_hidden()
        .bg(rgb(theme::BACKGROUND))
        .text_color(rgb(theme::TEXT))
        .font_family(APP_FONT_FAMILY)
        .text_size(APP_TEXT_SIZE)
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .overflow_y_scrollbar()
                .child(
                    div()
                        .w_full()
                        .min_h(scroll_content_min_height)
                        .flex()
                        .items_center()
                        .justify_center()
                        .px(px(24.0))
                        .py(vertical_padding)
                        .child(stage),
                ),
        )
}

fn render_wallet_brand_block(window: &Window, layout: WalletHeroLayout) -> gpui::Div {
    let viewport = window.viewport_size();
    let show_mineral = layout != WalletHeroLayout::Narrow;
    let mineral_size = match layout {
        WalletHeroLayout::Wide => (viewport.height * 0.42).min(px(500.0)).max(px(360.0)),
        WalletHeroLayout::Medium => (viewport.width * 0.24).min(px(320.0)).max(px(210.0)),
        WalletHeroLayout::Narrow => px(0.0),
    };
    let wordmark_width = match layout {
        WalletHeroLayout::Wide => px(400.0),
        WalletHeroLayout::Medium => (viewport.width * 0.44).min(px(360.0)).max(px(260.0)),
        WalletHeroLayout::Narrow => (viewport.width * 0.66).min(px(360.0)).max(px(220.0)),
    };
    let wordmark_height = wordmark_width * (23.0 / 166.0);
    let art_size = mineral_size * 1.5;
    let horizontal_mineral_offset = (art_size - mineral_size) / 2.0;
    let vertical_glow_offset = (mineral_size - art_size) / 2.0;

    div()
        .flex()
        .flex_col()
        .items_center()
        .gap_6()
        .when(show_mineral, |this| {
            this.child(
                div()
                    .relative()
                    .w(art_size)
                    .h(mineral_size)
                    .child(
                        img(WARM_GLOW_PATH)
                            .absolute()
                            .top(vertical_glow_offset)
                            .left_0()
                            .size(art_size)
                            .object_fit(ObjectFit::Fill),
                    )
                    .child(
                        img(HEMATITE_HERO_PATH)
                            .absolute()
                            .top_0()
                            .left(horizontal_mineral_offset)
                            .size(mineral_size)
                            .object_fit(ObjectFit::Contain),
                    ),
            )
        })
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .child(
                    img(HERO_WORDMARK_PATH)
                        .w(wordmark_width)
                        .h(wordmark_height)
                        .object_fit(ObjectFit::Contain),
                )
                .child(render_wallet_build_metadata()),
        )
}

fn render_wallet_build_metadata() -> gpui::Div {
    let build_label = wallet_build_label();

    div()
        .w_full()
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .gap_1()
                .child(
                    div()
                        .font_family(APP_MONO_FONT_FAMILY)
                        .text_size(px(12.0))
                        .line_height(px(16.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(build_label.clone()),
                )
                .child(clipboard_with_toast(
                    "wallet-hero-build-info-copy",
                    build_label,
                )),
        )
        .child(
            div()
                .w_full()
                .flex()
                .justify_center()
                .gap_1()
                .child(render_wallet_social_copy_button(
                    "wallet-hero-repository-url-copy",
                    Icon::new(IconName::GitHub).size_4(),
                    RAILOXIDE_REPOSITORY_URL,
                ))
                .child(render_wallet_social_copy_button(
                    "wallet-hero-telegram-url-copy",
                    Icon::new(RailgunSocialIcon::Telegram).size_4(),
                    TELEGRAM_URL,
                )),
        )
}

fn render_wallet_social_copy_button(
    id: &'static str,
    icon: impl IntoElement,
    url: &'static str,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_color(rgb(theme::TEXT_MUTED))
        .cursor_pointer()
        .hover(|this| {
            this.bg(rgb_with_alpha(theme::SURFACE_HOVER, 0.24))
                .text_color(rgb(theme::TEXT))
        })
        .tooltip(|window, cx| Tooltip::new(COPY_URL_TOOLTIP).build(window, cx))
        .on_click(move |_event, window, cx| {
            copy_to_clipboard_with_custom_toast(url, LINK_COPIED_MESSAGE, window, cx);
        })
        .child(icon)
}

pub(super) fn wallet_build_label() -> SharedString {
    SharedString::from(format!(
        "v{} {}",
        env!("CARGO_PKG_VERSION"),
        option_env!("RAILOXIDE_GIT_SHORT_HASH").unwrap_or("unknown")
    ))
}

impl WalletRoot {
    pub(super) fn render_workspace(
        &mut self,
        root: Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let active_content = self.render_active_content(&root, window, cx);
        if self.logs_open {
            let logs_content = self.render_logs_drawer(root);
            div().size_full().min_w(px(0.0)).min_h(px(0.0)).child(
                v_resizable("wallet-logs-drawer")
                    .with_state(&self.drawer_split)
                    .child(
                        resizable_panel().child(
                            div()
                                .size_full()
                                .min_w(px(0.0))
                                .min_h(px(0.0))
                                .child(active_content),
                        ),
                    )
                    .child(
                        resizable_panel()
                            .size(LOGS_DRAWER_HEIGHT)
                            .size_range(LOGS_DRAWER_MIN_HEIGHT..LOGS_DRAWER_MAX_HEIGHT)
                            .child(
                                div()
                                    .size_full()
                                    .min_w(px(0.0))
                                    .min_h(px(0.0))
                                    .child(logs_content),
                            ),
                    ),
            )
        } else {
            div()
                .size_full()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .child(active_content)
        }
    }

    fn render_active_content(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::AnyElement {
        match self.active_activity {
            Activity::Wallet => self.render_wallet_view(root, window).into_any_element(),
            Activity::Broadcaster => self.render_broadcaster_view(root).into_any_element(),
            Activity::AddressBook => self.render_address_book_view(root),
            Activity::Proposals => self
                .render_governance_workspace(root, window, cx)
                .into_any_element(),
            Activity::Settings => self.render_settings_view().into_any_element(),
        }
    }

    fn render_settings_view(&self) -> impl IntoElement {
        let content = if let Some(editor) = self.settings_editor.as_ref() {
            div().size_full().child(editor.clone()).into_any_element()
        } else {
            div()
                .p(px(24.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(
                    self.settings_error.as_ref().map_or_else(
                        || "Settings are unavailable".to_string(),
                        ToString::to_string,
                    ),
                ))
                .into_any_element()
        };
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .bg(rgb(theme::SURFACE))
            .p(px(16.0))
            .child(content)
    }

    fn render_wallet_view(&self, root: &Entity<Self>, window: &Window) -> impl IntoElement {
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .child(self.render_wallet_header(root))
            .child(self.render_wallet_tabs(root))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .p(px(12.0))
                    .child(self.render_wallet_content(root, window)),
            )
            .children(self.render_wallet_status_bar(root))
    }

    fn render_wallet_status_bar(&self, root: &Entity<Self>) -> Option<gpui::AnyElement> {
        let state = self.chain_states.get(&self.selected_chain)?;
        let counts = self.wallet_status_counts(
            state.snapshot().map(AsRef::as_ref),
            state.ppoi_workflow_status(),
        );
        let syncing = state.is_syncing();
        if !state.renders_table() {
            return None;
        }

        let chips = self.render_wallet_status_chips(root, state, counts);
        if syncing {
            let context = match state {
                ChainUtxoState::Loading { .. } => SyncStatusContext::Loading,
                ChainUtxoState::Syncing { .. } => SyncStatusContext::Syncing,
                ChainUtxoState::Idle
                | ChainUtxoState::Ready { .. }
                | ChainUtxoState::Error { .. } => return None,
            };
            Some(sync_status_bar(context, state.progress(), chips).into_any_element())
        } else {
            Some(ready_status_bar(counts, chips).into_any_element())
        }
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
            .get(&self.selected_chain)
            .map(|chain| chain.block_time)
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

    fn retry_selected_poi_artifact_cache_refresh(&mut self, cx: &mut Context<'_, Self>) {
        let chain_id = self.selected_chain;
        let Some(request_token) = self.poi_artifact_cache_retry_attempts.begin(chain_id) else {
            return;
        };
        cx.notify();

        let Some(session) = self
            .chain_states
            .get(&chain_id)
            .and_then(ChainUtxoState::poi_refresh_session)
        else {
            self.poi_artifact_cache_retry_attempts
                .cancel_pending(chain_id, request_token);
            cx.notify();
            return;
        };
        let retry_wallet_generation = self.active_wallet_generation;
        let retry_session = Arc::downgrade(&session);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = self.runtime.spawn(async move {
            let retry = match session.retry_poi_artifact_cache().await {
                Ok(retry) => retry,
                Err(error) => {
                    let _ = event_tx.send(PoiArtifactCacheRetryTaskEvent::AdmissionFailed(
                        format!("{error:#}"),
                    ));
                    return;
                }
            };
            let attempt_id = retry.attempt_id();
            if event_tx
                .send(PoiArtifactCacheRetryTaskEvent::Admitted(attempt_id))
                .is_err()
            {
                return;
            }
            let result = retry
                .wait()
                .await
                .map(|_| ())
                .map_err(|error| format!("{error:#}"));
            let _ = event_tx.send(PoiArtifactCacheRetryTaskEvent::Finished { attempt_id, result });
        });
        self.wallet_sync_lifecycle.track_wallet_task(task);
        cx.spawn(async move |root, cx| {
            while let Some(event) = event_rx.recv().await {
                match event {
                    PoiArtifactCacheRetryTaskEvent::AdmissionFailed(error) => {
                        let _ = root.update(cx, |root, cx| {
                            let attempt_released = root
                                .poi_artifact_cache_retry_attempts
                                .cancel_pending(chain_id, request_token);
                            if attempt_released {
                                cx.notify();
                            }
                            let session_is_current = root
                                .chain_states
                                .get(&chain_id)
                                .and_then(ChainUtxoState::poi_refresh_session)
                                .is_some_and(|current| {
                                    Weak::ptr_eq(&Arc::downgrade(&current), &retry_session)
                                });
                            if attempt_released
                                && ppoi_retry_completion_is_current(
                                    root.active_wallet_generation,
                                    retry_wallet_generation,
                                    session_is_current,
                                )
                            {
                                tracing::warn!(
                                    chain_id,
                                    %error,
                                    "failed to admit PPOI corpus refresh retry"
                                );
                            }
                        });
                    }
                    PoiArtifactCacheRetryTaskEvent::Admitted(attempt_id) => {
                        let _ = root.update(cx, |root, cx| {
                            if root.poi_artifact_cache_retry_attempts.bind(
                                chain_id,
                                request_token,
                                attempt_id,
                            ) {
                                cx.notify();
                            }
                        });
                    }
                    PoiArtifactCacheRetryTaskEvent::Finished { attempt_id, result } => {
                        let _ = root.update(cx, |root, cx| {
                            let attempt_released = root
                                .poi_artifact_cache_retry_attempts
                                .finish(chain_id, attempt_id);
                            if attempt_released {
                                cx.notify();
                            }
                            let session_is_current = root
                                .chain_states
                                .get(&chain_id)
                                .and_then(ChainUtxoState::poi_refresh_session)
                                .is_some_and(|current| {
                                    Weak::ptr_eq(&Arc::downgrade(&current), &retry_session)
                                });
                            if !attempt_released
                                || !ppoi_retry_completion_is_current(
                                    root.active_wallet_generation,
                                    retry_wallet_generation,
                                    session_is_current,
                                )
                            {
                                return;
                            }
                            if let Err(error) = result {
                                tracing::warn!(
                                    chain_id,
                                    %error,
                                    "failed to retry PPOI corpus refresh"
                                );
                            }
                        });
                    }
                }
            }
        })
        .detach();
    }

    fn render_wallet_status_chips(
        &self,
        root: &Entity<Self>,
        state: &ChainUtxoState,
        counts: WalletStatusCounts,
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
    ) -> gpui::AnyElement {
        let details_root = root.clone();
        render_ppoi_status_hover_target(root, "wallet-status-ppoi-hover")
            .cursor_pointer()
            .on_click(move |_event, window, cx| {
                details_root.update(cx, |root, cx| {
                    root.open_private_pending_status_dialog(window, cx);
                });
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

    fn render_wallet_tabs(&self, root: &Entity<Self>) -> impl IntoElement {
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
            .on_click(move |index, _window, cx| {
                let Some(tab) = WalletTab::ALL.get(*index).copied() else {
                    return;
                };
                tab_root.update(cx, |root, cx| {
                    root.select_wallet_tab(tab, cx);
                });
            })
            .children(WalletTab::ALL.into_iter().map(|tab| {
                Tab::new()
                    .min_w(px(92.0))
                    .label(tab.label())
                    .prefix(
                        Icon::empty()
                            .path(tab.icon_path())
                            .with_size(px(18.0))
                            .text_color(rgb(theme::TEXT)),
                    )
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

    fn render_wallet_content(&self, root: &Entity<Self>, window: &Window) -> gpui::AnyElement {
        match self.active_wallet_tab {
            WalletTab::Private => self.render_private_assets_body(root),
            WalletTab::Public => self.render_public_wallet_body(root),
            WalletTab::Activity => self.render_utxo_body(root, window).into_any_element(),
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
                                let chain_id = root.selected_chain;
                                let overrides = chain_load_overrides();
                                root.start_chain_load(chain_id, &overrides, true, cx);
                            });
                        }),
                )
            })
    }

    fn render_logs_drawer(&self, root: Entity<Self>) -> impl IntoElement {
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .border_t_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .h(px(34.0))
                    .flex()
                    .items_center()
                    .px(px(12.0))
                    .bg(rgb(theme::SURFACE))
                    .border_b_1()
                    .border_color(rgb(theme::BORDER))
                    .child(img(icons::logs_icon_path()).size(px(16.0)).flex_none())
                    .child(
                        div()
                            .ml(px(8.0))
                            .text_color(rgb(theme::TEXT))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Logs"),
                    )
                    .child(div().flex_1())
                    .child(
                        app_button_base("close-wallet-logs-drawer")
                            .ghost()
                            .xsmall()
                            .tooltip("Hide logs")
                            .icon(IconName::Close)
                            .on_click(move |_event, _window, cx| {
                                root.update(cx, |root, cx| {
                                    root.logs_open = false;
                                    cx.notify();
                                });
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .child(self.logs.clone()),
            )
    }
}

pub(super) const fn ppoi_retry_completion_is_current(
    current_wallet_generation: u64,
    retry_wallet_generation: u64,
    session_is_current: bool,
) -> bool {
    current_wallet_generation == retry_wallet_generation && session_is_current
}

fn status_presence_text(label: impl Into<SharedString>, status: PresenceStatus) -> gpui::Div {
    div()
        .h(px(24.0))
        .min_w_0()
        .px_1()
        .flex()
        .items_center()
        .gap_1()
        .text_color(rgb(theme::TEXT))
        .child(status_presence_dot(status))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(label.into()),
        )
}

fn render_balances_status_hover_target(
    root: &Entity<WalletRoot>,
    id: &'static str,
) -> gpui::Stateful<gpui::Div> {
    let tooltip_root = root.clone();
    div().id(id).hoverable_tooltip(move |_window, cx| {
        let root = tooltip_root.clone();
        cx.new(|cx| BalancesStatusHoverCard::new(root, cx)).into()
    })
}

struct BalancesStatusHoverCard {
    root: Entity<WalletRoot>,
}

impl BalancesStatusHoverCard {
    fn new(root: Entity<WalletRoot>, cx: &mut Context<'_, Self>) -> Self {
        cx.observe(&root, |_this, _root, cx| cx.notify()).detach();
        Self { root }
    }
}

impl Render for BalancesStatusHoverCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let now = now_epoch_secs();
        let (status, labels, sync_tip, data_source, issue, counts, network_mode) = {
            let root = self.root.read(cx);
            let chain_id = root.selected_chain;
            let block_time = root
                .effective_chain_configs
                .get(&chain_id)
                .map(|chain| chain.block_time);
            let state = root.chain_states.get(&chain_id);
            let counts = root.wallet_status_counts(
                state.and_then(ChainUtxoState::snapshot).map(AsRef::as_ref),
                state.map_or_else(
                    WalletPpoiWorkflowStatus::default,
                    ChainUtxoState::ppoi_workflow_status,
                ),
            );
            let context = state.and_then(|state| match state {
                ChainUtxoState::Loading { .. } => Some(SyncStatusContext::Loading),
                ChainUtxoState::Syncing { .. } => Some(SyncStatusContext::Syncing),
                ChainUtxoState::Idle
                | ChainUtxoState::Ready { .. }
                | ChainUtxoState::Error { .. } => None,
            });
            let progress = state.and_then(ChainUtxoState::progress);
            let labels = context.map(|context| sync_status_labels(context, progress));
            let status = state.map_or(PresenceStatus::Unknown, |state| {
                root.balances_status_for_state(state)
            });
            let sync_tip = state.and_then(ChainUtxoState::sync_tip);
            let data_source = balance_sync_data_source(context, progress);
            let issue = state
                .filter(|state| matches!(state, ChainUtxoState::Ready { .. }))
                .and_then(|_| block_time.and_then(|time| balance_sync_issue(sync_tip, time, now)));
            (
                status,
                labels,
                sync_tip,
                data_source,
                issue,
                counts,
                root.http.network_mode(),
            )
        };
        let color = presence_status_color(status);

        div()
            .w(px(360.0))
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap_3()
            .text_size(APP_TEXT_SIZE)
            .text_color(rgb(theme::TEXT))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(status_presence_dot(status).flex_none())
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(color))
                            .child(balances_hover_heading(status, labels.as_ref(), issue)),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(18.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(balances_hover_detail(
                        status,
                        labels.as_ref(),
                        issue,
                        network_mode,
                    )),
            )
            .when_some(labels.as_ref(), |this, labels| {
                this.child(render_balance_sync_progress_section(labels, data_source))
            })
            .when_some(sync_tip, |this, sync_tip| {
                this.child(render_balance_sync_tip_section(sync_tip, now))
            })
            .when_some(balance_pending_detail(counts), |this, detail| {
                this.child(render_status_hover_note_base(
                    "Balance updates pending",
                    &detail,
                    theme::WARNING,
                    0.08,
                ))
            })
    }
}

fn render_ppoi_status_hover_target(
    root: &Entity<WalletRoot>,
    id: &'static str,
) -> gpui::Stateful<gpui::Div> {
    let tooltip_root = root.clone();
    div().id(id).hoverable_tooltip(move |_window, cx| {
        let root = tooltip_root.clone();
        cx.new(|cx| PpoiStatusHoverCard::new(root, cx)).into()
    })
}

struct PpoiStatusHoverCard {
    root: Entity<WalletRoot>,
}

impl PpoiStatusHoverCard {
    fn new(root: Entity<WalletRoot>, cx: &mut Context<'_, Self>) -> Self {
        cx.observe(&root, |_this, _root, cx| cx.notify()).detach();
        Self { root }
    }
}

impl Render for PpoiStatusHoverCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let (status, progress, refreshing, counts, retrying) = {
            let root = self.root.read(cx);
            let state = root.chain_states.get(&root.selected_chain);
            let counts = root.wallet_status_counts(
                state.and_then(ChainUtxoState::snapshot).map(AsRef::as_ref),
                state.map_or_else(
                    WalletPpoiWorkflowStatus::default,
                    ChainUtxoState::ppoi_workflow_status,
                ),
            );
            let status = state.map_or(PresenceStatus::Unknown, |state| {
                root.ppoi_status_for_state(state, counts)
            });
            let refreshing = state.is_some_and(ChainUtxoState::poi_refreshing);
            let progress = root.selected_chain_poi_artifact_progress().cloned();
            let retrying = root
                .poi_artifact_cache_retry_attempts
                .contains(root.selected_chain);
            (status, progress, refreshing, counts, retrying)
        };
        let color = presence_status_color(status);
        let event_label = ppoi_event_header_label(progress.as_ref());

        div()
            .w(px(360.0))
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap_3()
            .text_size(APP_TEXT_SIZE)
            .text_color(rgb(theme::TEXT))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(status_presence_dot(status).flex_none())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(color))
                                    .truncate()
                                    .child(ppoi_hover_heading(
                                        status,
                                        progress.as_ref(),
                                        refreshing,
                                    )),
                            )
                            .when_some(event_label, |this, label| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .text_color(rgb(theme::TEXT_MUTED))
                                        .child(format!("({label})")),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(18.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .when_some(
                        ppoi_hover_detail(status, progress.as_ref(), refreshing),
                        gpui::ParentElement::child,
                    ),
            )
            .when_some(
                progress
                    .as_ref()
                    .filter(|progress| progress.total_lists > 1),
                |this, progress| this.child(render_ppoi_list_progress_section(progress)),
            )
            .when_some(
                progress.as_ref().filter(|progress| !progress.is_ready()),
                |this, progress| {
                    if progress.is_error() {
                        this.child(render_ppoi_artifact_error_section(
                            self.root.clone(),
                            progress,
                            status,
                            retrying,
                        ))
                    } else {
                        this.child(render_ppoi_artifact_progress_section(progress, status))
                    }
                },
            )
            .when(refreshing, |this| {
                this.child(render_ppoi_hover_note(
                    "Submitting PPOIs…",
                    "Submitting sender-created contexts and checking owned outputs. Open Private asset status to review or retry blocked work.",
                    theme::WARNING,
                ))
            })
            .when(
                counts.ppoi_workflow_status.has_outstanding() && !refreshing,
                |this| {
                    this.child(render_ppoi_hover_note(
                        ppoi_workflow_status_title(counts.ppoi_workflow_status, false)
                            .unwrap_or("Outgoing proof recovery"),
                        &ppoi_workflow_status_detail(counts.ppoi_workflow_status),
                        if counts.ppoi_workflow_status.needs_attention > 0
                            || counts.ppoi_workflow_status.recovery_needs_attention > 0
                        {
                            theme::DANGER
                        } else {
                            theme::WARNING
                        },
                    ))
                },
            )
            .when(counts.ppoi_attention_count() > 0, |this| {
                this.child(render_ppoi_hover_action_note(
                    self.root.clone(),
                    "Needs review",
                    &ppoi_attention_detail(counts),
                    ppoi_attention_hover_color(counts),
                ))
            })
    }
}

fn render_ppoi_artifact_progress_section(
    progress: &PoiArtifactCacheProgress,
    status: PresenceStatus,
) -> gpui::Div {
    let percent = progress.percent();
    let completed_lists = if percent == 100 && progress.total_lists > 0 {
        progress.total_lists
    } else {
        progress.completed_lists.min(progress.total_lists)
    };
    let list_count = if progress.total_lists == 1 {
        "list"
    } else {
        "lists"
    };
    let list_text = if progress.total_lists == 0 {
        "Preparing POI list metadata".to_string()
    } else if progress.total_lists == 1 && completed_lists == 1 {
        "POI list ready".to_string()
    } else {
        format!(
            "{} of {} {} ready",
            completed_lists, progress.total_lists, list_count
        )
    };
    let color = presence_status_color(status);

    div()
        .rounded_md()
        .border_1()
        .border_color(rgb_with_alpha(color, 0.24))
        .bg(rgb_with_alpha(color, 0.05))
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(color))
                        .child(ppoi_artifact_phase_label(progress.phase)),
                )
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(color))
                        .child(format!("{percent}%")),
                ),
        )
        .child(
            UiProgress::new()
                .h(px(7.0))
                .value(f32::from(percent))
                .bg(rgb(color)),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(list_text),
        )
        .when_some(
            ppoi_chunk_progress_label(progress),
            |this, chunk_progress| {
                this.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(chunk_progress),
                )
            },
        )
        .when_some(
            ppoi_replay_progress_label(progress),
            |this, replay_progress| {
                this.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(replay_progress),
                )
            },
        )
        .when(
            progress.current_event_index.is_some() || progress.target_event_index.is_some(),
            |this| {
                this.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(ppoi_event_progress_label(progress)),
                )
            },
        )
        .when_some(progress.current_list_key.as_ref(), |this, list_key| {
            this.child(
                div()
                    .font_family(APP_MONO_FONT_FAMILY)
                    .text_size(px(12.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(format!("List {}", short_poi_list_key(list_key.as_slice()))),
            )
        })
        .when_some(progress.last_error.as_ref(), |this, error| {
            this.child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(17.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(format!("Last error: {error}")),
            )
        })
}

fn render_ppoi_artifact_error_section(
    root: Entity<WalletRoot>,
    progress: &PoiArtifactCacheProgress,
    status: PresenceStatus,
    retrying: bool,
) -> gpui::Div {
    let color = if status == PresenceStatus::Error {
        theme::DANGER
    } else {
        theme::WARNING
    };
    let error = progress
        .last_error
        .clone()
        .unwrap_or_else(|| "Artifact cache refresh failed.".to_string());

    div()
        .rounded_md()
        .border_1()
        .border_color(rgb_with_alpha(color, 0.34))
        .bg(rgb_with_alpha(color, 0.05))
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(color))
                .child("Last refresh failed"),
        )
        .child(
            div()
                .text_size(px(12.0))
                .line_height(px(17.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(error),
        )
        .child(
            div().flex().justify_end().child(
                app_button("wallet-status-ppoi-retry-artifact-cache", "Retry refresh")
                    .small()
                    .loading(retrying)
                    .disabled(retrying)
                    .on_click(move |_event, _window, cx| {
                        cx.stop_propagation();
                        root.update(cx, |root, cx| {
                            root.retry_selected_poi_artifact_cache_refresh(cx);
                        });
                    }),
            ),
        )
}

fn render_ppoi_hover_note(title: &str, detail: &str, color: u32) -> gpui::Div {
    render_ppoi_hover_note_base(title, detail, color, 0.08)
}

fn render_ppoi_hover_action_note(
    root: Entity<WalletRoot>,
    title: &'static str,
    detail: &str,
    color: u32,
) -> gpui::Stateful<gpui::Div> {
    render_ppoi_hover_note_base(title, detail, color, 0.08)
        .id("wallet-status-ppoi-needs-review")
        .cursor_pointer()
        .hover(move |this| this.bg(rgb_with_alpha(color, 0.14)))
        .on_click(move |_event, window, cx| {
            cx.stop_propagation();
            root.update(cx, |root, cx| {
                root.open_private_pending_status_dialog(window, cx);
            });
        })
}

fn render_ppoi_hover_note_base(title: &str, detail: &str, color: u32, bg_alpha: f32) -> gpui::Div {
    render_status_hover_note_base(title, detail, color, bg_alpha)
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
                .font_weight(gpui::FontWeight::SEMIBOLD)
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

fn balances_hover_heading(
    status: PresenceStatus,
    labels: Option<&SyncStatusLabels>,
    issue: Option<BalanceSyncIssue>,
) -> String {
    if let Some(issue) = issue {
        return balance_sync_issue_heading(issue).to_string();
    }
    if let Some(labels) = labels {
        return labels.title.clone();
    }
    match status {
        PresenceStatus::Healthy => "Balances ready",
        PresenceStatus::Active => "Balances catching up",
        PresenceStatus::Error => "Balance sync error",
        PresenceStatus::Unknown => "Balances unavailable",
    }
    .to_string()
}

fn balances_hover_detail(
    status: PresenceStatus,
    labels: Option<&SyncStatusLabels>,
    issue: Option<BalanceSyncIssue>,
    network_mode: WalletNetworkMode,
) -> String {
    if let Some(issue) = issue {
        return balance_sync_issue_detail(issue, network_mode);
    }
    if labels.is_some() {
        return "Private balance sync is catching up with chain state.".to_string();
    }
    match status {
        PresenceStatus::Healthy => "Private balances are synced and following chain state.",
        PresenceStatus::Active => "Private balance sync is catching up with chain state.",
        PresenceStatus::Error => "Private balance sync reported an error.",
        PresenceStatus::Unknown => "Private balance sync state is not available yet.",
    }
    .to_string()
}

fn render_balance_sync_progress_section(
    labels: &SyncStatusLabels,
    data_source: Option<PublicScanSource>,
) -> gpui::Div {
    div()
        .rounded_md()
        .border_1()
        .border_color(rgb_with_alpha(theme::WARNING, 0.34))
        .bg(rgb_with_alpha(theme::WARNING, 0.05))
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    UiProgress::new()
                        .flex_1()
                        .h(px(7.0))
                        .value(f32::from(labels.percent))
                        .bg(rgb(theme::WARNING)),
                )
                .child(
                    div()
                        .w(px(42.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::WARNING))
                        .child(format!("{}%", labels.percent)),
                ),
        )
        .child(
            div()
                .text_size(px(12.0))
                .line_height(px(17.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(labels.detail.clone()),
        )
        .when_some(data_source, |section, source| {
            section.child(render_balance_sync_tip_row(
                "Data source",
                balance_sync_source_label(source).to_string(),
            ))
        })
}

pub(super) const fn balance_sync_source_label(source: PublicScanSource) -> &'static str {
    match source {
        PublicScanSource::CachedCoverage => "Local cache",
        PublicScanSource::IndexedArtifacts => "Verified artifacts",
        PublicScanSource::Squid => "Squid index",
        PublicScanSource::Rpc => "RPC",
        PublicScanSource::ArchiveRpc => "Archive RPC",
    }
}

pub(super) const fn balance_sync_data_source(
    context: Option<SyncStatusContext>,
    progress: Option<wallet_ops::SyncProgressUpdate>,
) -> Option<PublicScanSource> {
    match (context, progress) {
        (Some(_), Some(progress)) => progress.source,
        _ => None,
    }
}

fn render_balance_sync_tip_section(sync_tip: WalletSyncTip, now_secs: u64) -> gpui::Div {
    div()
        .rounded_md()
        .border_1()
        .border_color(rgb_with_alpha(theme::BORDER, 0.72))
        .bg(rgb_with_alpha(theme::SURFACE_ELEVATED, 0.34))
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(theme::TEXT))
                .child("Chain position"),
        )
        .child(render_balance_sync_tip_row(
            "Wallet state",
            format_block_label(sync_tip.last_scanned_block),
        ))
        .child(render_balance_sync_tip_row(
            "Safe head",
            format_block_label(sync_tip.safe_head_block),
        ))
        .child(render_balance_sync_tip_row(
            "RPC head",
            format_block_label(sync_tip.head_block),
        ))
        .when_some(
            sync_tip.head_last_advanced_at_unix_secs,
            |this, advanced_at| {
                this.child(render_balance_sync_tip_row(
                    "Head advanced",
                    ui::format::format_relative_age(Duration::from_secs(
                        now_secs.saturating_sub(advanced_at),
                    )),
                ))
            },
        )
}

fn render_balance_sync_tip_row(label: &'static str, value: String) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .text_size(px(12.0))
        .child(
            div()
                .min_w_0()
                .text_color(rgb(theme::TEXT_MUTED))
                .truncate()
                .child(label),
        )
        .child(
            div()
                .flex_none()
                .font_family(APP_MONO_FONT_FAMILY)
                .text_color(rgb(theme::TEXT))
                .child(value),
        )
}

const fn balance_sync_issue_heading(issue: BalanceSyncIssue) -> &'static str {
    match issue {
        BalanceSyncIssue::HeadUnavailable => "Balance head unavailable",
        BalanceSyncIssue::HeadStalled { .. } => "Balance source stale",
        BalanceSyncIssue::Lagging { .. } => "Balances lagging",
    }
}

pub(super) fn balance_sync_issue_detail(
    issue: BalanceSyncIssue,
    network_mode: WalletNetworkMode,
) -> String {
    match issue {
        BalanceSyncIssue::HeadUnavailable => "Waiting for chain head updates.".to_string(),
        BalanceSyncIssue::HeadStalled {
            stale_secs,
            threshold_secs: _,
        } => format!(
            "RPC head has not advanced for {}. {}",
            ui::format::format_compact_duration(Duration::from_secs(stale_secs)),
            balance_sync_issue_suggestion(network_mode)
        ),
        BalanceSyncIssue::Lagging {
            lag_blocks,
            threshold_blocks: _,
        } => format!(
            "Wallet state is {lag_blocks} safe-head blocks behind. {}",
            balance_sync_issue_suggestion(network_mode)
        ),
    }
}

const fn balance_sync_issue_suggestion(network_mode: WalletNetworkMode) -> &'static str {
    match network_mode {
        WalletNetworkMode::Tor => "Consider generating a new Tor session or using premium RPCs.",
        WalletNetworkMode::Proxy | WalletNetworkMode::Direct => "Consider using premium RPCs.",
    }
}

fn balance_pending_detail(counts: WalletStatusCounts) -> Option<String> {
    let mut parts = Vec::new();
    if counts.pending_incoming_outputs > 0 {
        parts.push(count_label(
            counts.pending_incoming_outputs,
            "incoming output",
        ));
    }
    if counts.pending_outgoing_outputs > 0 {
        parts.push(count_label(
            counts.pending_outgoing_outputs,
            "outgoing output",
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!(
            "{} waiting for confirmation and safe-head finality.",
            parts.join(" · ")
        ))
    }
}

fn format_block_label(block: Option<u64>) -> String {
    block.map_or_else(|| "Waiting".to_string(), |block| format!("block {block}"))
}

pub(super) fn ppoi_hover_heading(
    status: PresenceStatus,
    progress: Option<&PoiArtifactCacheProgress>,
    refreshing: bool,
) -> &'static str {
    if let Some(progress) = progress {
        if progress.is_error() {
            return match progress.failure_kind() {
                Some(PoiArtifactCacheFailureKind::ServingCorpusUnavailable) => {
                    "PPOI data unavailable"
                }
                None if status == PresenceStatus::Error => "PPOI checks blocked",
                Some(PoiArtifactCacheFailureKind::RefreshDegraded) | None => {
                    "Artifact cache refresh failed"
                }
            };
        }
        if progress.is_active() {
            if progress.ready_for_wallet_checks {
                return "Refreshing PPOI data";
            }
            return match progress.phase {
                PoiArtifactCachePhase::LoadingPersisted => "Loading saved PPOI data",
                PoiArtifactCachePhase::Resetting => "Resetting PPOI data",
                PoiArtifactCachePhase::ResolvingManifest => "Resolving POI manifest",
                PoiArtifactCachePhase::VerifyingCatalog => "Verifying POI catalog",
                PoiArtifactCachePhase::Planning => "Planning POI refresh",
                PoiArtifactCachePhase::DownloadingChunks => "Downloading POI chunks",
                PoiArtifactCachePhase::ReplayingRanges => "Replaying POI event ranges",
                PoiArtifactCachePhase::Validating => "Validating PPOI data",
                PoiArtifactCachePhase::Persisting => "Saving PPOI data",
                PoiArtifactCachePhase::LiveTailing => "Following POI event tail",
                PoiArtifactCachePhase::Idle
                | PoiArtifactCachePhase::Ready
                | PoiArtifactCachePhase::Failed => "PPOI catching up",
            };
        }
    }
    if refreshing {
        return "Submitting PPOIs…";
    }
    match status {
        PresenceStatus::Healthy => "PPOI ready",
        PresenceStatus::Active => "PPOI action needed",
        PresenceStatus::Error => "PPOI checks blocked",
        PresenceStatus::Unknown => "PPOI status unavailable",
    }
}

pub(super) fn ppoi_hover_detail(
    status: PresenceStatus,
    progress: Option<&PoiArtifactCacheProgress>,
    refreshing: bool,
) -> Option<&'static str> {
    if let Some(progress) = progress {
        if progress.is_error() {
            return match progress.failure_kind() {
                Some(PoiArtifactCacheFailureKind::RefreshDegraded) => Some(
                    "Refresh failed; wallet checks continue using verified PPOI data saved on this device.",
                ),
                Some(PoiArtifactCacheFailureKind::ServingCorpusUnavailable) => {
                    Some("No verified PPOI data is available for wallet checks.")
                }
                None if progress.ready_for_wallet_checks => Some(
                    "Refresh failed; wallet checks continue using verified PPOI data saved on this device.",
                ),
                None => Some("No verified PPOI data is available for wallet checks."),
            };
        }
        if progress.is_active() {
            return progress.ready_for_wallet_checks.then_some(
                "Refreshing in the background while wallet checks use verified PPOI data saved on this device.",
            );
        }
    }
    if refreshing {
        return Some(
            "Submitting sender-created contexts and checking owned private-output PPOI status.",
        );
    }
    match status {
        PresenceStatus::Healthy => Some("Up to date and following the source."),
        PresenceStatus::Active => {
            Some("PPOI data is ready; one or more outputs still need proof submission or recovery.")
        }
        PresenceStatus::Error => Some("PPOI checks are blocked until the artifact cache rebuilds."),
        PresenceStatus::Unknown => {
            Some("PPOI source or artifact-cache status is not available yet.")
        }
    }
}

const fn ppoi_artifact_phase_label(phase: PoiArtifactCachePhase) -> &'static str {
    match phase {
        PoiArtifactCachePhase::Idle => "Idle",
        PoiArtifactCachePhase::LoadingPersisted => "Loading saved PPOI data",
        PoiArtifactCachePhase::Resetting => "Resetting PPOI data",
        PoiArtifactCachePhase::ResolvingManifest => "Resolving manifest",
        PoiArtifactCachePhase::VerifyingCatalog => "Verifying catalog",
        PoiArtifactCachePhase::Planning => "Planning refresh",
        PoiArtifactCachePhase::DownloadingChunks => "Downloading chunks",
        PoiArtifactCachePhase::ReplayingRanges => "Replaying ranges",
        PoiArtifactCachePhase::Validating => "Validating PPOI data",
        PoiArtifactCachePhase::Persisting => "Saving PPOI data",
        PoiArtifactCachePhase::LiveTailing => "Live tailing",
        PoiArtifactCachePhase::Ready => "Ready",
        PoiArtifactCachePhase::Failed => "Failed",
    }
}

pub(super) fn ppoi_chunk_progress_label(progress: &PoiArtifactCacheProgress) -> Option<String> {
    if progress.graph.total_chunks == 0 {
        return None;
    }
    let mut label = format!(
        "{}/{} chunks",
        progress
            .graph
            .verified_chunks
            .min(progress.graph.total_chunks),
        progress.graph.total_chunks
    );
    if let Some(total_bytes) = progress.graph.total_authenticated_encoded_bytes {
        let _ = write!(
            label,
            " · {}/{}",
            format_binary_bytes(progress.graph.verified_encoded_bytes.min(total_bytes)),
            format_binary_bytes(total_bytes)
        );
    } else if progress.graph.verified_encoded_bytes > 0 {
        let _ = write!(
            label,
            " · {} verified",
            format_binary_bytes(progress.graph.verified_encoded_bytes)
        );
    }
    Some(label)
}

pub(super) fn ppoi_replay_progress_label(progress: &PoiArtifactCacheProgress) -> Option<String> {
    let start = progress.graph.replay_start_event_index?;
    let end = progress.graph.replay_end_event_index?;
    let replayed = progress
        .graph
        .replayed_event_count
        .min(progress.graph.total_replay_event_count);
    Some(format!(
        "Events {start}-{end} · {replayed}/{} replayed",
        progress.graph.total_replay_event_count
    ))
}

fn ppoi_event_progress_label(progress: &PoiArtifactCacheProgress) -> String {
    match (progress.current_event_index, progress.target_event_index) {
        (Some(current), Some(target)) if current >= target => format!("Event {target}"),
        (Some(current), Some(target)) => format!("Event {} of {}", current.min(target), target),
        (Some(current), None) => format!("Event {current}"),
        (None, Some(target)) => format!("Target event {target}"),
        (None, None) => String::new(),
    }
}

fn ppoi_event_header_label(progress: Option<&PoiArtifactCacheProgress>) -> Option<String> {
    let progress = progress?;
    if progress.total_lists != 1 {
        return None;
    }
    if let [list] = progress.list_progress.as_slice() {
        return ppoi_inline_event_label(list.current_event_index, list.target_event_index);
    }
    ppoi_inline_event_label(progress.current_event_index, progress.target_event_index)
}

fn ppoi_inline_event_label(current: Option<u64>, target: Option<u64>) -> Option<String> {
    match (current, target) {
        (Some(current), Some(target)) if current < target => {
            Some(format!("event {current}/{target}"))
        }
        (Some(current), Some(target)) => Some(format!("event {}", current.min(target))),
        (Some(current), None) => Some(format!("event {current}")),
        (None, Some(target)) => Some(format!("event {target}")),
        (None, None) => None,
    }
}

fn render_ppoi_list_progress_section(progress: &PoiArtifactCacheProgress) -> gpui::Div {
    let ready_lists = progress
        .list_progress
        .iter()
        .filter(|progress| progress.ready_for_wallet_checks)
        .count();

    div()
        .rounded_md()
        .border_1()
        .border_color(rgb_with_alpha(theme::BORDER, 0.72))
        .bg(rgb_with_alpha(theme::SURFACE_ELEVATED, 0.34))
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::TEXT))
                        .child("POI lists"),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(format!(
                            "{} of {} ready",
                            ready_lists.min(progress.total_lists),
                            progress.total_lists,
                        )),
                ),
        )
        .children(
            progress
                .list_progress
                .iter()
                .map(render_ppoi_list_progress_row),
        )
}

fn render_ppoi_list_progress_row(progress: &PoiArtifactCacheListProgress) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .text_size(px(12.0))
        .child(
            div()
                .min_w_0()
                .font_family(APP_MONO_FONT_FAMILY)
                .text_color(rgb(theme::TEXT_MUTED))
                .truncate()
                .child(short_poi_list_key(progress.list_key.as_slice())),
        )
        .child(
            div()
                .flex_none()
                .font_family(APP_MONO_FONT_FAMILY)
                .text_color(rgb(theme::TEXT_MUTED))
                .child(ppoi_list_event_label(progress).unwrap_or_else(|| "Not ready".to_string())),
        )
}

fn ppoi_list_event_label(progress: &PoiArtifactCacheListProgress) -> Option<String> {
    match (progress.current_event_index, progress.target_event_index) {
        (Some(current), Some(target)) if current < target => {
            Some(format!("Event {current}/{target}"))
        }
        (Some(current), Some(target)) => Some(format!("Event {}", current.min(target))),
        (Some(current), None) => Some(format!("Event {current}")),
        (None, Some(target)) => Some(format!("Event {target}")),
        (None, None) => None,
    }
}

fn short_poi_list_key(bytes: &[u8]) -> String {
    let encoded = hex::encode(bytes);
    if encoded.len() <= 16 {
        return encoded;
    }
    format!("{}...{}", &encoded[..8], &encoded[encoded.len() - 6..])
}

const fn ppoi_attention_badge_color(counts: WalletStatusCounts) -> u32 {
    if counts.blocked_shield_outputs > 0
        || counts.ppoi_workflow_status.needs_attention > 0
        || counts.ppoi_workflow_status.recovery_needs_attention > 0
    {
        theme::DANGER
    } else {
        theme::WARNING_BG
    }
}

const fn ppoi_attention_hover_color(counts: WalletStatusCounts) -> u32 {
    if counts.blocked_shield_outputs > 0
        || counts.ppoi_workflow_status.needs_attention > 0
        || counts.ppoi_workflow_status.recovery_needs_attention > 0
    {
        theme::DANGER
    } else {
        theme::WARNING
    }
}

fn ppoi_attention_detail(counts: WalletStatusCounts) -> String {
    let mut items = Vec::with_capacity(2);
    if counts.blocked_shield_outputs > 0 {
        items.push(count_label(
            counts.blocked_shield_outputs,
            "blocked Shield output",
        ));
    }
    let recovery_attention = counts.recoverable_poi_outputs.saturating_add(
        usize::try_from(
            counts
                .ppoi_workflow_status
                .needs_attention
                .saturating_add(counts.ppoi_workflow_status.recovery_needs_attention),
        )
        .unwrap_or(usize::MAX),
    );
    if recovery_attention > 0 {
        items.push(count_label(recovery_attention, "PPOI output needing retry"));
    }
    format!("Review {}", items.join(" and "))
}

fn status_presence_dot(status: PresenceStatus) -> gpui::Div {
    if status == PresenceStatus::Healthy {
        return healthy_presence_dot();
    }
    div()
        .size(px(7.0))
        .rounded_full()
        .bg(rgb(presence_status_color(status)))
}

fn healthy_presence_dot() -> gpui::Div {
    const SLOT_SIZE: f32 = 15.0;

    div()
        .relative()
        .size(px(SLOT_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .absolute()
                .top(px(3.0))
                .left(px(3.0))
                .size(px(9.0))
                .rounded_full()
                .bg(rgb_with_alpha(theme::SUCCESS, 0.38))
                .opacity(0.52),
        )
        .child(div().size(px(6.0)).rounded_full().bg(rgb(theme::SUCCESS)))
}

const fn presence_status_color(status: PresenceStatus) -> u32 {
    match status {
        PresenceStatus::Healthy => theme::SUCCESS,
        PresenceStatus::Active => theme::WARNING,
        PresenceStatus::Error => theme::DANGER,
        PresenceStatus::Unknown => theme::TEXT_MUTED,
    }
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
