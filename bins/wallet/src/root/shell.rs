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
use wallet_ops::settings::EffectiveChainRegistry;

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
mod status_bar;
mod wallet_tabs;
mod workspace_views;

mod tab_navigation;
mod window_frame;

pub(super) use window_frame::*;

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

/// The tab a new root opens on: the default where the initial chain has Railgun, else Public.
pub(super) fn initial_wallet_tab(
    effective_chain_configs: &EffectiveChainRegistry,
    initial_chain_id: u64,
) -> WalletTab {
    if effective_chain_configs.railgun(initial_chain_id).is_ok() {
        WalletTab::default()
    } else {
        WalletTab::Public
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
