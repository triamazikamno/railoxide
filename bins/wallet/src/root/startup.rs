use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use broadcaster_monitor::{EventRx, EventTx, Shared, publish_revision};
use broadcaster_monitor_waku::{RelayNetworkConfig, WakuMonitorConfig, WakuMonitorDirectPeer};
use eyre::WrapErr;
use gpui::{
    App, AppContext, Context, Entity, IntoElement, MouseDownEvent, MouseMoveEvent, ParentElement,
    Render, ScrollEvent, SharedString, Styled, Subscription, Window, canvas, div, img,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, IconName, Sizable, WindowExt, button::ButtonVariants,
    progress::Progress as UiProgress, spinner::Spinner,
};
use tokio::runtime::Handle;
use tokio::sync::watch;
use ui::controls::{app_button, app_button_base, app_muted_text, app_strong_text};
use ui::dialog::WalletDialogExt as _;
use ui::icons;
use ui::logs::{LogStore, LogsPane};
use ui::theme;
use wallet_ops::{
    BroadcasterFeePolicy, HttpContext, PoiReadSource, TokenAnchorRateCache, WalletNetworkConfig,
    WalletNetworkMode, WalletNetworkProgress, WalletNetworkProgressStage,
    build_wallet_network_context_with_progress, request_tor_state_reset,
    settings::{
        EffectiveChainRegistry, EffectiveTokenRegistry, WalletSettings,
        build_effective_chain_configs, build_effective_token_registry, default_waku_backup_peers,
        default_waku_direct_peers, load_wallet_settings, load_wallet_ui_state,
        save_wallet_settings,
    },
    spawn_token_anchor_refresh_worker,
    vault::DesktopVaultStore,
};

use super::chain_load::WalletRootReplacementCleanup;
use super::settings::{
    StartupSettingsSummary, WalletSettingsEditor, data_folder_path, data_folder_row,
    open_data_folder, settings_dialog_dimensions, startup_settings_action_state,
};
use super::shell::{WalletAppOptions, render_wallet_hero_screen, render_wallet_window_frame};
use super::{
    WalletMaintenanceController, WalletRoot, WalletRootDependencies, format_report_chain,
    rgb_with_alpha,
};

mod first_start;
mod startup_retry;
mod startup_splash;
mod startup_tasks;

/// Keeps the tracing target of the moved methods at this module's path.
const LOG_TARGET: &str = module_path!();

pub(in crate::root) struct WalletStartupReady {
    pub(in crate::root) http: HttpContext,
    pub(in crate::root) waku_config: WakuMonitorConfig,
    pub(in crate::root) monitor_event_tx: EventTx,
    pub(in crate::root) vault_store: Arc<DesktopVaultStore>,
    pub(in crate::root) chain_ids: Vec<u64>,
    pub(in crate::root) initial_chain_id: u64,
    pub(in crate::root) ui_state: wallet_ops::settings::WalletUiState,
    pub(in crate::root) effective_chain_configs: EffectiveChainRegistry,
    pub(in crate::root) effective_token_registry: EffectiveTokenRegistry,
    pub(in crate::root) public_balance_refresh_interval: Duration,
    pub(in crate::root) auto_lock_timeout: Option<Duration>,
    pub(in crate::root) public_broadcaster_policy: BroadcasterFeePolicy,
    pub(in crate::root) public_broadcaster_response_timeout: Duration,
    pub(in crate::root) public_broadcaster_republish_interval: Duration,
    pub(in crate::root) default_allow_suspicious_broadcasters: bool,
    pub(in crate::root) mimic_railway_shields_by_default: bool,
    pub(in crate::root) poi_read_source: PoiReadSource,
}

enum StartupNetworkContext {
    Build,
    Reuse(Box<HttpContext>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum NetworkContextPlan {
    Build,
    ReuseActive,
    RetainActiveAndBuild,
    ReuseRetained,
}

pub(in crate::root) fn network_context_plan(
    active_mode: Option<WalletNetworkMode>,
    target_mode: WalletNetworkMode,
    has_retained_tor: bool,
) -> NetworkContextPlan {
    if active_mode == Some(target_mode) {
        return NetworkContextPlan::ReuseActive;
    }
    if target_mode == WalletNetworkMode::Tor && has_retained_tor {
        return NetworkContextPlan::ReuseRetained;
    }
    if active_mode == Some(WalletNetworkMode::Tor) {
        return NetworkContextPlan::RetainActiveAndBuild;
    }
    NetworkContextPlan::Build
}

const TOR_BOOTSTRAP_RECOVERY_DELAY: Duration = Duration::from_secs(5);
const TOR_RESET_TOOLTIP: &str = "The wallet closes now and rebuilds its Tor connections when you reopen it. Only Tor's cached data is cleared.";
type WalletActivityCallback = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

pub(in crate::root) const fn tor_bootstrap_recovery_is_current(
    expected_generation: u64,
    current_generation: u64,
    stage: WalletNetworkProgressStage,
) -> bool {
    expected_generation == current_generation
        && matches!(stage, WalletNetworkProgressStage::BootstrappingTor)
}

pub(super) struct WalletStartupRoot {
    options: WalletAppOptions,
    runtime: Handle,
    monitor_state: Shared,
    event_tx: EventTx,
    event_rx: EventRx,
    chain_ids: Vec<u64>,
    logs: LogStore,
    progress: WalletNetworkProgress,
    error: Option<Arc<str>>,
    vault_store: Option<Arc<DesktopVaultStore>>,
    wallet_root: Option<Entity<WalletRoot>>,
    public_transaction_tracker: wallet_ops::PublicTransactionTracker,
    maintenance_controller: Entity<WalletMaintenanceController>,
    startup_generation: u64,
    retained_tor_http: Option<HttpContext>,
    tor_bootstrap_recovery_available: bool,
    tor_reset_error: Option<Arc<str>>,
    /// The last Open failure of the startup Settings dialog's data folder row.
    data_folder_open_error: Option<SharedString>,
    /// First-start setup, shown instead of network startup until it is saved.
    setup: Option<first_start::FirstStartSetup>,
    _activity_keystroke_interceptor: Subscription,
}

impl Render for WalletStartupRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let titlebar_color = self
            .wallet_root
            .as_ref()
            .map_or(theme::BACKGROUND, |root| root.read(cx).titlebar_color());
        let content = if let Some(root) = self.wallet_root.as_ref() {
            div().size_full().child(root.clone()).into_any_element()
        } else if self.setup.is_some() {
            self.sync_first_start_proxy_input(window, cx);
            self.render_first_start(window, cx)
        } else {
            self.render_splash(window, cx)
        };

        let activity_observer = self.wallet_root.clone().map(|root| {
            canvas(
                |_, _window, _cx| {},
                move |_, (), window, _cx| {
                    let callback: WalletActivityCallback = Rc::new({
                        move |window, cx| {
                            root.update(cx, |root, cx| {
                                root.handle_wallet_activity(
                                    super::auto_lock::WalletActivitySource::Desktop,
                                    window,
                                    cx,
                                )
                            })
                        }
                    });
                    register_wallet_activity_listeners(window, callback);
                },
            )
            .absolute()
            .size_full()
        });
        div()
            .relative()
            .size_full()
            .children(activity_observer)
            .child(render_wallet_window_frame(content, window, titlebar_color))
    }
}

fn is_retained_tor_context(http: &HttpContext) -> bool {
    http.network_mode() == WalletNetworkMode::Tor
}

fn register_wallet_activity_listeners(window: &mut Window, callback: WalletActivityCallback) {
    let move_callback = Rc::clone(&callback);
    window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
        if phase.capture() && move_callback(window, cx) {
            cx.stop_propagation();
        }
    });
    let click_callback = Rc::clone(&callback);
    window.on_mouse_event(move |_: &MouseDownEvent, phase, window, cx| {
        if phase.capture() && click_callback(window, cx) {
            cx.stop_propagation();
        }
    });
    window.on_mouse_event(move |_: &ScrollEvent, phase, window, cx| {
        if phase.capture() && callback(window, cx) {
            cx.stop_propagation();
        }
    });
}

fn reusable_http_context_matches_settings(
    http: &HttpContext,
    settings: &WalletSettings,
    proxy_url: Option<&reqwest::Url>,
) -> bool {
    if http.network_mode() != settings.wallet_network_mode() {
        return false;
    }
    match settings.wallet_network_mode() {
        WalletNetworkMode::Proxy => http.user_proxy_url.as_ref() == proxy_url,
        WalletNetworkMode::Tor | WalletNetworkMode::Direct => proxy_url.is_none(),
    }
}

pub(super) fn load_validated_startup_settings(
    vault_store: &DesktopVaultStore,
) -> eyre::Result<WalletSettings> {
    let db = vault_store.db();
    let settings = load_wallet_settings(db.as_ref()).wrap_err("load wallet settings")?;
    settings
        .validate()
        .map_err(|error| eyre::eyre!("wallet settings are invalid: {error}"))?;
    Ok(settings)
}

pub(super) fn resolve_initial_chain_id(
    enabled_chain_ids: &[u64],
    remembered_chain_id: Option<u64>,
) -> u64 {
    remembered_chain_id
        .filter(|chain_id| enabled_chain_ids.contains(chain_id))
        .unwrap_or_else(|| {
            enabled_chain_ids
                .first()
                .copied()
                .expect("validated wallet settings must enable at least one chain")
        })
}

#[cfg(test)]
mod activity_tests;
