use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use broadcaster_monitor::{EventRx, EventTx, Shared, publish_revision};
use broadcaster_monitor_waku::{RelayNetworkConfig, WakuMonitorConfig, WakuMonitorDirectPeer};
use eyre::WrapErr;
use gpui::{
    App, AppContext, Context, Entity, IntoElement, MouseDownEvent, MouseMoveEvent, ParentElement,
    Render, ScrollWheelEvent, SharedString, Styled, Subscription, Window, canvas, div, img,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, IconName, Root, Sizable, WindowExt, button::ButtonVariants,
    progress::Progress as UiProgress, spinner::Spinner,
};
use tokio::runtime::Handle;
use tokio::sync::watch;
use ui::controls::{app_button, app_button_base, app_muted_text, app_strong_text};
use ui::icons;
use ui::logs::{LogStore, LogsPane};
use ui::theme;
use wallet_ops::{
    BroadcasterFeePolicy, HttpContext, PoiReadSource, TokenAnchorRateCache, WalletNetworkConfig,
    WalletNetworkMode, WalletNetworkProgress, WalletNetworkProgressStage,
    build_wallet_network_context_with_progress, request_tor_state_reset,
    settings::{
        EffectiveChainConfig, EffectiveTokenRegistry, WalletSettings,
        build_effective_chain_configs, build_effective_token_registry, default_waku_direct_peers,
        load_wallet_settings, load_wallet_ui_state, save_wallet_settings,
    },
    spawn_token_anchor_refresh_worker,
    vault::DesktopVaultStore,
};

use super::chain_load::WalletRootReplacementCleanup;
use super::settings::{
    StartupSettingsSummary, WalletSettingsEditor, settings_dialog_dimensions,
    startup_settings_action_state,
};
use super::shell::{WalletAppOptions, render_wallet_hero_screen, render_wallet_window_frame};
use super::{
    WalletMaintenanceController, WalletRoot, format_report_chain, rgb_with_alpha,
    scrollable_dialog_content,
};

struct WalletStartupReady {
    http: HttpContext,
    waku_config: WakuMonitorConfig,
    monitor_event_tx: EventTx,
    vault_store: Arc<DesktopVaultStore>,
    chain_ids: Vec<u64>,
    initial_chain_id: u64,
    ui_state: wallet_ops::settings::WalletUiState,
    effective_chain_configs: BTreeMap<u64, EffectiveChainConfig>,
    effective_token_registry: EffectiveTokenRegistry,
    public_balance_refresh_interval: Duration,
    auto_lock_timeout: Option<Duration>,
    public_broadcaster_policy: BroadcasterFeePolicy,
    public_broadcaster_response_timeout: Duration,
    public_broadcaster_republish_interval: Duration,
    default_allow_suspicious_broadcasters: bool,
    mimic_railway_shields_by_default: bool,
    poi_read_source: PoiReadSource,
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
    maintenance_controller: Entity<WalletMaintenanceController>,
    startup_generation: u64,
    retained_tor_http: Option<HttpContext>,
    tor_bootstrap_recovery_available: bool,
    tor_reset_error: Option<Arc<str>>,
    _activity_keystroke_interceptor: Subscription,
}

impl WalletStartupRoot {
    pub(super) fn new(
        options: WalletAppOptions,
        runtime: Handle,
        monitor_state: Shared,
        event_tx: EventTx,
        event_rx: EventRx,
        chain_ids: &[u64],
        logs: LogStore,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let chain_ids = chain_ids.to_vec();
        let progress = WalletNetworkProgress::initial();
        let (vault_store, error) = match DesktopVaultStore::open(options.db_path.clone()) {
            Ok(store) => (Some(Arc::new(store)), None),
            Err(error) => (
                None,
                Some(Arc::from(format!(
                    "Failed to open wallet database: {error}"
                ))),
            ),
        };
        let maintenance_controller = cx.new({
            let runtime = runtime.clone();
            move |_| WalletMaintenanceController::new(runtime)
        });
        cx.observe(&maintenance_controller, |_root, _controller, cx| {
            cx.notify();
        })
        .detach();
        let wallet_window = window.window_handle();
        let root_entity = cx.weak_entity();
        let activity_keystroke_interceptor =
            cx.intercept_keystrokes(move |_event, event_window, cx| {
                if event_window.window_handle() != wallet_window {
                    return;
                }
                let locked = root_entity
                    .update(cx, |startup, cx| {
                        startup.wallet_root.as_ref().is_some_and(|root| {
                            root.update(cx, |root, cx| {
                                root.handle_wallet_activity(event_window, cx)
                            })
                        })
                    })
                    .unwrap_or(false);
                if locked {
                    cx.stop_propagation();
                }
            });
        let root = Self {
            options,
            runtime,
            monitor_state,
            event_tx: event_tx.clone(),
            event_rx,
            chain_ids,
            logs,
            progress,
            error,
            vault_store: vault_store.clone(),
            wallet_root: None,
            maintenance_controller,
            startup_generation: 1,
            retained_tor_http: None,
            tor_bootstrap_recovery_available: false,
            tor_reset_error: None,
            _activity_keystroke_interceptor: activity_keystroke_interceptor,
        };
        if let Some(vault_store) = vault_store {
            let (progress_tx, progress_rx) = watch::channel(root.progress.clone());
            root.spawn_startup_tasks(
                1,
                event_tx,
                StartupNetworkContext::Build,
                None,
                progress_tx,
                progress_rx,
                vault_store,
                window,
                cx,
            );
        }
        root
    }

    fn spawn_startup_tasks(
        &self,
        generation: u64,
        event_tx: EventTx,
        network_context: StartupNetworkContext,
        cleanup: Option<WalletRootReplacementCleanup>,
        progress_tx: watch::Sender<WalletNetworkProgress>,
        mut progress_rx: watch::Receiver<WalletNetworkProgress>,
        vault_store: Arc<DesktopVaultStore>,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while progress_rx.changed().await.is_ok() {
                let progress = progress_rx.borrow().clone();
                if this
                    .update(cx, |root, cx| {
                        if root.startup_generation != generation {
                            return;
                        }
                        root.update_network_progress(progress, generation, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let options = self.options.clone();
        let chain_ids = self.chain_ids.clone();
        let startup = self.runtime.spawn(async move {
            if let Some(cleanup) = cleanup {
                cleanup.wait().await.map_err(|error| eyre::eyre!(error))?;
            }
            build_wallet_startup(
                options,
                chain_ids,
                event_tx,
                network_context,
                progress_tx,
                vault_store,
            )
            .await
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = startup.await;
            let _ = this.update_in(cx, |root, window, cx| match result {
                _ if root.startup_generation != generation => {
                    tracing::debug!(generation, "ignoring stale wallet startup result");
                }
                Ok(Ok(ready)) => root.finish_startup(ready, window, cx),
                Ok(Err(error)) => root.fail_startup(format_report_chain(&error), cx),
                Err(error) => root.fail_startup(format!("Wallet startup task failed: {error}"), cx),
            });
            let _ = this.update(cx, |root, cx| {
                root.maintenance_controller.update(cx, |controller, cx| {
                    controller.clear_finished_root_replacement_cleanup();
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn update_network_progress(
        &mut self,
        progress: WalletNetworkProgress,
        generation: u64,
        cx: &mut Context<'_, Self>,
    ) {
        let entered_tor_bootstrap = self.progress.stage
            != WalletNetworkProgressStage::BootstrappingTor
            && progress.stage == WalletNetworkProgressStage::BootstrappingTor;
        self.progress = progress;
        if self.progress.stage != WalletNetworkProgressStage::BootstrappingTor {
            self.tor_bootstrap_recovery_available = false;
        }
        if entered_tor_bootstrap {
            self.tor_bootstrap_recovery_available = false;
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(TOR_BOOTSTRAP_RECOVERY_DELAY)
                    .await;
                let _ = this.update(cx, |root, cx| {
                    if tor_bootstrap_recovery_is_current(
                        generation,
                        root.startup_generation,
                        root.progress.stage,
                    ) {
                        root.tor_bootstrap_recovery_available = true;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn finish_startup(
        &mut self,
        ready: WalletStartupReady,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let ready_is_tor = ready.http.network_mode() == WalletNetworkMode::Tor;
        let event_rx = self.event_rx.clone();
        let logs = self.logs.clone();
        let monitor_state = self.monitor_state.clone();
        let public_broadcaster_anchor_cache = Arc::new(TokenAnchorRateCache::new());
        let enabled_chain_ids = ready.chain_ids.clone();
        let anchor_effective_chains = ready.effective_chain_configs.clone();
        let anchor_token_registry = ready.effective_token_registry.clone();
        let public_broadcaster_anchor_refresh = spawn_token_anchor_refresh_worker(
            &self.runtime,
            Arc::clone(&public_broadcaster_anchor_cache),
            enabled_chain_ids.clone(),
            anchor_effective_chains,
            anchor_token_registry,
            ready.http.clone(),
        );
        let fee_anchor_lookup: broadcaster_monitor_gpui::FeeAnchorLookup = Arc::new({
            let public_broadcaster_anchor_cache = Arc::clone(&public_broadcaster_anchor_cache);
            move |chain_id, token| public_broadcaster_anchor_cache.cached_rate(chain_id, token)
        });
        let wallet_monitor_event_rx = event_rx.clone();
        let initial_chain_id = ready.initial_chain_id;
        let monitor_default_fee_tokens = ready
            .effective_chain_configs
            .values()
            .filter_map(|chain| {
                chain
                    .wrapped_native_token
                    .as_deref()
                    .and_then(|token| token.parse().ok())
                    .map(|token| (chain.chain_id, token))
            })
            .collect();
        let monitor = cx.new(|cx| {
            broadcaster_monitor_gpui::BroadcasterMonitorPane::new(
                self.monitor_state.clone(),
                event_rx,
                &enabled_chain_ids,
                initial_chain_id,
                monitor_default_fee_tokens,
                fee_anchor_lookup,
                window,
                cx,
            )
        });
        let logs = cx.new(|cx| LogsPane::new(logs, window, cx));
        let startup_root = cx.weak_entity();
        let maintenance_controller = self.maintenance_controller.clone();
        let root = cx.new(|cx| {
            WalletRoot::new(
                self.options.clone(),
                ready.http,
                ready.vault_store,
                &enabled_chain_ids,
                initial_chain_id,
                ready.ui_state,
                ready.effective_chain_configs,
                ready.effective_token_registry,
                ready.public_balance_refresh_interval,
                ready.auto_lock_timeout,
                ready.public_broadcaster_policy,
                ready.public_broadcaster_response_timeout,
                ready.public_broadcaster_republish_interval,
                ready.default_allow_suspicious_broadcasters,
                ready.mimic_railway_shields_by_default,
                ready.poi_read_source,
                self.runtime.clone(),
                monitor_state,
                ready.waku_config,
                ready.monitor_event_tx,
                public_broadcaster_anchor_cache,
                public_broadcaster_anchor_refresh,
                wallet_monitor_event_rx,
                monitor,
                logs,
                &startup_root,
                &maintenance_controller,
                window,
                cx,
            )
        });
        self.error = None;
        self.wallet_root = Some(root);
        if ready_is_tor {
            self.retained_tor_http = None;
        }
        cx.notify();
    }

    fn retain_tor_context(&mut self, http: HttpContext) {
        if !is_retained_tor_context(&http) {
            return;
        }
        let generation = http.tor_session_generation();
        tracing::debug!(generation, "retaining warm Tor network context");
        let _ = self.retained_tor_http.replace(http);
    }

    fn reuse_retained_tor_context(&self) -> Option<HttpContext> {
        let http = self.retained_tor_http.as_ref()?.clone();
        let generation = http.tor_session_generation();
        tracing::debug!(generation, "reusing retained Tor network context");
        Some(http)
    }

    fn fail_startup(&mut self, message: String, cx: &mut Context<'_, Self>) {
        tracing::error!(error = %message, "wallet startup failed");
        self.error = Some(Arc::from(message));
        cx.notify();
    }

    fn startup_vault_store(&mut self) -> Result<Arc<DesktopVaultStore>, String> {
        if let Some(store) = self.vault_store.as_ref() {
            return Ok(Arc::clone(store));
        }
        match DesktopVaultStore::open(self.options.db_path.clone()) {
            Ok(store) => {
                let store = Arc::new(store);
                self.vault_store = Some(Arc::clone(&store));
                Ok(store)
            }
            Err(error) => Err(format!("Failed to open wallet database: {error}")),
        }
    }

    pub(super) fn retry_startup(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if !self.maintenance_controller.read(cx).is_idle() {
            return;
        }
        self.retry_startup_with_network_context(None, window, cx);
    }

    pub(super) fn root_replacement_is_allowed(&self, cx: &App) -> bool {
        self.wallet_root
            .as_ref()
            .is_none_or(|root| root.read(cx).root_replacement_is_allowed())
    }

    pub(super) fn retry_startup_with_network_context(
        &mut self,
        reusable_http: Option<HttpContext>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.maintenance_controller.read(cx).is_idle() {
            return;
        }
        if !self.root_replacement_is_allowed(cx) {
            return;
        }
        let vault_store = match self.startup_vault_store() {
            Ok(store) => store,
            Err(message) => {
                self.wallet_root = None;
                self.fail_startup(message, cx);
                return;
            }
        };
        let settings = match load_validated_startup_settings(&vault_store) {
            Ok(settings) => settings,
            Err(error) => {
                self.fail_startup(format_report_chain(&error), cx);
                return;
            }
        };
        let replacement = self
            .wallet_root
            .as_ref()
            .map(|root| root.update(cx, super::WalletRoot::begin_root_replacement_shutdown));
        let (cleanup, outgoing_http) =
            replacement.map_or((None, None), |(cleanup, http)| (Some(cleanup), Some(http)));
        let active_http = match outgoing_http {
            Some(http) => {
                drop(reusable_http);
                Some(http)
            }
            None => reusable_http,
        };
        let active_mode = active_http.as_ref().map(HttpContext::network_mode);
        let has_retained_tor = self
            .retained_tor_http
            .as_ref()
            .is_some_and(is_retained_tor_context);
        if self
            .retained_tor_http
            .as_ref()
            .is_some_and(|http| !is_retained_tor_context(http))
        {
            self.retained_tor_http = None;
        }
        let target_mode = settings.wallet_network_mode();
        let network_context = match network_context_plan(active_mode, target_mode, has_retained_tor)
        {
            NetworkContextPlan::Build => {
                drop(active_http);
                StartupNetworkContext::Build
            }
            NetworkContextPlan::ReuseActive => {
                let http = active_http.expect("active network context required for reuse");
                if target_mode == WalletNetworkMode::Tor {
                    self.retained_tor_http = Some(http.clone());
                }
                StartupNetworkContext::Reuse(Box::new(http))
            }
            NetworkContextPlan::RetainActiveAndBuild => {
                if let Some(http) = active_http {
                    self.retain_tor_context(http);
                }
                StartupNetworkContext::Build
            }
            NetworkContextPlan::ReuseRetained => {
                drop(active_http);
                self.reuse_retained_tor_context()
                    .map_or(StartupNetworkContext::Build, |http| {
                        StartupNetworkContext::Reuse(Box::new(http))
                    })
            }
        };
        self.startup_generation = self.startup_generation.saturating_add(1);
        self.maintenance_controller.update(cx, |controller, _cx| {
            controller.clear_active_root();
            controller.set_root_replacement_cleanup(cleanup.clone());
        });
        self.wallet_root = None;
        self.error = None;
        self.tor_reset_error = None;
        self.tor_bootstrap_recovery_available = false;
        self.progress = WalletNetworkProgress::initial();
        if cleanup.is_none()
            && let Some(rev) = self.monitor_state.write().clear()
        {
            publish_revision(&self.event_tx, rev);
        }
        let (progress_tx, progress_rx) = watch::channel(self.progress.clone());
        self.spawn_startup_tasks(
            self.startup_generation,
            self.event_tx.clone(),
            network_context,
            cleanup,
            progress_tx,
            progress_rx,
            vault_store,
            window,
            cx,
        );
        cx.notify();
    }

    fn reset_settings_and_retry(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if !self.maintenance_controller.read(cx).is_idle() {
            return;
        }
        let store = match self.startup_vault_store() {
            Ok(store) => store,
            Err(message) => {
                self.fail_startup(message, cx);
                return;
            }
        };
        let db = store.db();
        match save_wallet_settings(db.as_ref(), &WalletSettings::default()) {
            Ok(()) => self.retry_startup(window, cx),
            Err(error) => {
                self.fail_startup(format!("Failed to reset wallet settings: {error}"), cx);
            }
        }
    }

    fn quit_and_reset_tor_state(&mut self, cx: &mut Context<'_, Self>) {
        if !self.tor_bootstrap_recovery_available {
            return;
        }
        match request_tor_state_reset(&self.options.db_path) {
            Ok(marker_path) => {
                tracing::warn!(
                    marker_path = %marker_path.display(),
                    "requested Tor state reset on next wallet startup; quitting wallet"
                );
                cx.quit();
            }
            Err(error) => {
                tracing::warn!(%error, "failed to request Tor state reset during startup");
                self.tor_reset_error = Some(Arc::from(format_report_chain(&error)));
                cx.notify();
            }
        }
    }

    fn open_startup_settings_dialog(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        window.close_all_dialogs(cx);
        let root = cx.entity();
        let (editor, summary) = match self.vault_store.clone() {
            Some(store) => {
                let db = store.db();
                match load_wallet_settings(db.as_ref()) {
                    Ok(settings) => {
                        let runtime = self.runtime.clone();
                        let startup_root = root.downgrade();
                        let maintenance_controller = self.maintenance_controller.clone();
                        (
                            Some(cx.new(move |cx| {
                                WalletSettingsEditor::new(
                                    store,
                                    runtime,
                                    settings,
                                    maintenance_controller,
                                    Some(startup_root),
                                    None,
                                    cx,
                                )
                            })),
                            None,
                        )
                    }
                    Err(error) => (
                        None,
                        Some(StartupSettingsSummary::error(format!(
                            "Failed to load wallet settings: {error}"
                        ))),
                    ),
                }
            }
            None => (
                None,
                Some(StartupSettingsSummary::error(
                    self.error.as_ref().map_or_else(
                        || "Wallet database is unavailable".to_string(),
                        ToString::to_string,
                    ),
                )),
            ),
        };
        let (dialog_width, content_height, dialog_max_height) = settings_dialog_dimensions(window);
        let maintenance_controller = self.maintenance_controller.clone();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let reset_root = root.clone();
            let retry_root = root.clone();
            let content = if let Some(editor) = editor.clone() {
                div()
                    .h(content_height)
                    .min_h(px(0.0))
                    .child(editor)
                    .into_any_element()
            } else {
                let summary = summary.clone().unwrap_or_else(|| {
                    StartupSettingsSummary::error("Settings are unavailable".to_string())
                });
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(app_muted_text(
                        "Settings are stored in the selected wallet database and are readable before vault unlock.",
                    ))
                    .child(summary.render())
                    .child(
                        div()
                            .mt(px(8.0))
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                app_button("startup-settings-reset", "Reset settings")
                                    .disabled(!maintenance_controller.read(&*cx).is_idle())
                                    .on_click(move |_event, window, cx| {
                                        window.close_all_dialogs(cx);
                                        reset_root.update(cx, |root, cx| {
                                            root.reset_settings_and_retry(window, cx);
                                        });
                                    }),
                            )
                            .child(
                                app_button("startup-settings-retry", "Retry startup")
                                    .primary()
                                    .disabled(!maintenance_controller.read(&*cx).is_idle())
                                    .on_click(move |_event, window, cx| {
                                        window.close_all_dialogs(cx);
                                        retry_root.update(cx, |root, cx| {
                                            root.retry_startup(window, cx);
                                        });
                                    }),
                            ),
                    )
                    .into_any_element()
            };
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .margin_top(px(16.0))
                .title(app_strong_text("Startup Settings"))
                .child(scrollable_dialog_content(content_height, content))
        });
    }

    fn render_splash(&self, window: &Window, cx: &Context<'_, Self>) -> gpui::AnyElement {
        let root = cx.entity();
        let has_error = self.error.is_some();
        let accent = if has_error {
            theme::DANGER
        } else {
            theme::INFO
        };
        let percent = self.progress.percent.unwrap_or(0);
        let maintenance_idle = self.maintenance_controller.read(cx).is_idle();
        let maintenance_status = self.maintenance_controller.read(cx).status();
        let action_state = startup_settings_action_state(has_error, maintenance_idle);
        let tor_reset_available = self.tor_bootstrap_recovery_available;
        let tor_reset_error = self.tor_reset_error.clone();
        let stage = if has_error {
            "Network startup failed"
        } else {
            self.progress.stage.label()
        };
        let detail = self
            .error
            .as_ref()
            .map_or_else(|| self.progress.detail.to_string(), ToString::to_string);
        let card = div()
            .w_full()
            .p(px(24.0))
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::BORDER_STRONG))
            .bg(rgb_with_alpha(theme::SURFACE_ELEVATED, 0.86))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(34.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(theme::SURFACE))
                            .border_1()
                            .border_color(rgb(accent))
                            .when(!has_error, |this| {
                                this.child(
                                    Spinner::new()
                                        .icon(IconName::LoaderCircle)
                                        .color(rgb(accent).into())
                                        .with_size(px(18.0)),
                                )
                            })
                            .when(has_error, |this| {
                                this.child(img(icons::globe_icon_path()).size(px(17.0)))
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(accent))
                                    .child(stage),
                            )
                            .child(
                                div()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(SharedString::from(detail)),
                            ),
                    ),
            )
            .when_some(maintenance_status, |this, status| {
                this.child(
                    div()
                        .mt(px(14.0))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .bg(rgb(theme::SURFACE))
                        .p(px(12.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(SharedString::from(status.to_string())),
                )
            })
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        UiProgress::new()
                            .flex_1()
                            .h(px(7.0))
                            .value(f32::from(percent)),
                    )
                    .child(
                        div()
                            .w(px(42.0))
                            .text_color(rgb(accent))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(SharedString::from(format!("{percent}%"))),
                    ),
            )
            .when(tor_reset_available, |this| {
                let reset_root = root.clone();
                this.child(
                    div()
                        .mt(px(14.0))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .when_some(tor_reset_error, |this, error| {
                            this.child(
                                div()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(theme::DANGER))
                                    .bg(rgb(theme::SURFACE))
                                    .p(px(10.0))
                                    .text_color(rgb(theme::DANGER))
                                    .child(SharedString::from(error.to_string())),
                            )
                        })
                        .child(
                            div().flex().justify_end().child(
                                app_button("wallet-startup-reset-tor-state", "Reset Tor state")
                                    .outline()
                                    .danger()
                                    .disabled(!maintenance_idle)
                                    .tooltip(TOR_RESET_TOOLTIP)
                                    .on_click(move |_event, _window, cx| {
                                        reset_root.update(cx, |root, cx| {
                                            root.quit_and_reset_tor_state(cx);
                                        });
                                    }),
                            ),
                        ),
                )
            })
            .when(action_state.reset || action_state.retry, |this| {
                let retry_root = root.clone();
                let reset_root = root.clone();
                this.child(
                    div()
                        .mt(px(14.0))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(theme::DANGER))
                        .bg(rgb(theme::SURFACE))
                        .p(px(12.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(
                            "Wallet networking failed closed. No direct network fallback was started.",
                        ),
                )
                .child(
                    div()
                        .mt(px(14.0))
                        .flex()
                        .gap_2()
                        .justify_end()
                        .when(action_state.reset, |this| {
                            this.child(
                                app_button("wallet-startup-reset-settings", "Reset settings")
                                    .disabled(!action_state.maintenance_actions_enabled)
                                    .on_click(move |_event, window, cx| {
                                        reset_root.update(cx, |root, cx| {
                                            root.reset_settings_and_retry(window, cx);
                                        });
                                    }),
                            )
                        })
                        .when(action_state.retry, |this| {
                            this.child(
                                app_button("wallet-startup-retry", "Retry startup")
                                    .primary()
                                    .disabled(!action_state.maintenance_actions_enabled)
                                    .on_click(move |_event, window, cx| {
                                        retry_root.update(cx, |root, cx| {
                                            root.retry_startup(window, cx);
                                        });
                                    }),
                            )
                        }),
                )
            })
            .into_any_element();

        render_wallet_hero_screen(window, card)
            .when(action_state.settings, |this| {
                this.child(Self::render_startup_settings_gear(root))
            })
            .into_any_element()
    }

    fn render_startup_settings_gear(root: Entity<Self>) -> gpui::Div {
        div().absolute().right(px(24.0)).bottom(px(24.0)).child(
            app_button_base("wallet-startup-settings")
                .outline()
                .h(px(40.0))
                .w(px(40.0))
                .tooltip("Settings")
                .icon(IconName::Settings)
                .on_click(move |_event, window, cx| {
                    root.update(cx, |root, cx| {
                        root.open_startup_settings_dialog(window, cx);
                    });
                }),
        )
    }

    pub(super) fn open_settings_from_shortcut(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(root) = self.wallet_root.clone() {
            root.update(cx, |root, cx| {
                root.open_settings_from_shortcut(window, cx);
            });
        } else {
            self.open_startup_settings_dialog(window, cx);
        }
    }

    pub(super) fn lock_vault_from_shortcut(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(root) = self.wallet_root.clone() {
            root.update(cx, |root, cx| {
                root.lock_vault_from_shortcut(window, cx);
            });
        }
    }
}

impl Render for WalletStartupRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let titlebar_color = self
            .wallet_root
            .as_ref()
            .map_or(theme::BACKGROUND, |root| root.read(cx).titlebar_color());
        let content = if let Some(root) = self.wallet_root.as_ref() {
            div().size_full().child(root.clone()).into_any_element()
        } else {
            self.render_splash(window, cx)
        };

        let activity_observer = self.wallet_root.clone().map(|root| {
            canvas(
                |_, _window, _cx| {},
                move |_, (), window, _cx| {
                    let callback: WalletActivityCallback = Rc::new({
                        move |window, cx| {
                            root.update(cx, |root, cx| root.handle_wallet_activity(window, cx))
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
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
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
    window.on_mouse_event(move |_: &ScrollWheelEvent, phase, window, cx| {
        if phase.capture() && callback(window, cx) {
            cx.stop_propagation();
        }
    });
}

async fn build_wallet_startup(
    options: WalletAppOptions,
    _chain_ids: Vec<u64>,
    event_tx: EventTx,
    network_context: StartupNetworkContext,
    progress_tx: watch::Sender<WalletNetworkProgress>,
    vault_store: Arc<DesktopVaultStore>,
) -> eyre::Result<WalletStartupReady> {
    let settings = load_validated_startup_settings(&vault_store)?;
    let ui_state =
        load_wallet_ui_state(vault_store.db().as_ref()).wrap_err("load wallet UI state")?;
    let proxy_url = settings
        .network
        .proxy_url
        .as_deref()
        .map(reqwest::Url::parse)
        .transpose()
        .wrap_err("parse wallet settings proxy URL")?;
    let chain_ids = settings.chains.enabled_chain_ids();
    let initial_chain_id = resolve_initial_chain_id(&chain_ids, ui_state.last_chain_id);
    let effective_chain_configs = build_effective_chain_configs(&settings)
        .map_err(|error| eyre::eyre!("wallet chain settings are invalid: {error}"))?;
    let effective_token_registry = build_effective_token_registry(&settings)
        .map_err(|error| eyre::eyre!("wallet token settings are invalid: {error}"))?;
    let poi_read_source = settings
        .poi_read_source()
        .map_err(|error| eyre::eyre!("wallet POI settings are invalid: {error}"))?;
    let http = startup_http_context(
        &options,
        &settings,
        proxy_url.as_ref(),
        network_context,
        progress_tx,
    )
    .await?;

    let waku_network = match http.network_mode() {
        WalletNetworkMode::Tor => {
            let tor_client = http
                .arti_client_provider()
                .ok_or_else(|| eyre::eyre!("Tor Waku profile requires an Arti client"))?;
            RelayNetworkConfig::tor_with_client_provider(tor_client, http.client.clone())
        }
        WalletNetworkMode::Proxy => RelayNetworkConfig::proxy(http.client.clone()),
        WalletNetworkMode::Direct => RelayNetworkConfig::direct(),
    };
    let waku_config = WakuMonitorConfig {
        chain_ids: chain_ids.clone(),
        cluster_id: Some(settings.waku.cluster_id),
        shard_id: Some(settings.waku.shard_id),
        dns_enr_trees: settings.waku.dns_enr_trees.clone(),
        direct_peers: settings
            .waku
            .direct_peers
            .clone()
            .unwrap_or_else(default_waku_direct_peers)
            .iter()
            .map(|peer| WakuMonitorDirectPeer {
                peer_id: peer.peer_id.clone(),
                addr: peer.addr.clone(),
            })
            .collect(),
        doh_endpoint: settings.waku.doh_endpoint.clone(),
        doh_fallback_endpoints: settings.waku.doh_fallback_endpoints.clone(),
        max_peers: Some(settings.waku.max_peers),
        peer_connection_timeout: Some(Duration::from_secs(
            settings.waku.peer_connection_timeout_secs,
        )),
        nwaku_url: settings.waku.nwaku_url.clone(),
        network: waku_network,
    };

    tracing::info!(
        chains = ?chain_ids,
        network_mode = %http.network_mode(),
        network_status = http.network_status_label(),
        network_detail = %http.network_status_detail(),
        "starting wallet"
    );

    Ok(WalletStartupReady {
        http,
        waku_config,
        monitor_event_tx: event_tx,
        vault_store,
        chain_ids,
        initial_chain_id,
        ui_state,
        effective_chain_configs,
        effective_token_registry,
        public_balance_refresh_interval: Duration::from_secs(
            settings.runtime.public_balance_refresh_interval_secs,
        ),
        auto_lock_timeout: settings
            .runtime
            .auto_lock_timeout_secs
            .map(Duration::from_secs),
        public_broadcaster_policy: settings.broadcaster.fee_policy(),
        public_broadcaster_response_timeout: Duration::from_secs(
            settings.broadcaster.response_timeout_secs,
        ),
        public_broadcaster_republish_interval: Duration::from_secs(
            settings.broadcaster.republish_interval_secs,
        ),
        default_allow_suspicious_broadcasters: settings
            .broadcaster
            .allow_suspicious_broadcasters_by_default,
        mimic_railway_shields_by_default: settings.privacy.mimic_railway_shields_by_default,
        poi_read_source,
    })
}

async fn startup_http_context(
    options: &WalletAppOptions,
    settings: &WalletSettings,
    proxy_url: Option<&reqwest::Url>,
    network_context: StartupNetworkContext,
    progress_tx: watch::Sender<WalletNetworkProgress>,
) -> eyre::Result<HttpContext> {
    if let StartupNetworkContext::Reuse(http) = network_context {
        if reusable_http_context_matches_settings(&http, settings, proxy_url) {
            tracing::info!(
                network_mode = %http.network_mode(),
                "reusing active wallet network context"
            );
            let _ = progress_tx.send(WalletNetworkProgress::new(
                Some(http.network_mode()),
                WalletNetworkProgressStage::Ready,
                Some(100),
                format!(
                    "Reusing active {} network context",
                    http.network_mode().as_str()
                ),
            ));
            return Ok(*http);
        }
        tracing::warn!(
            active_network_mode = %http.network_mode(),
            settings_network_mode = %settings.wallet_network_mode(),
            "active wallet network context does not match settings; rebuilding"
        );
    }

    build_wallet_network_context_with_progress(
        WalletNetworkConfig {
            network_mode: Some(settings.wallet_network_mode()),
            proxy: proxy_url,
            data_dir: &options.db_path,
        },
        progress_tx,
    )
    .await
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
mod activity_tests {
    use std::cell::Cell;

    use gpui::{
        FocusHandle, InteractiveElement as _, KeyDownEvent, Keystroke, Modifiers, MouseButton,
        ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase, point,
    };

    use super::*;

    struct ActivityObserverProbe {
        count: Rc<Cell<usize>>,
        stop_activity: Rc<Cell<bool>>,
        content_clicks: Rc<Cell<usize>>,
        root_focus: FocusHandle,
        dialog_focus: FocusHandle,
        dialog_open: bool,
        _keystroke_interceptor: Subscription,
    }

    impl Render for ActivityObserverProbe {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            let count = Rc::clone(&self.count);
            let stop_activity = Rc::clone(&self.stop_activity);
            let observer = canvas(
                |_, _window, _cx| {},
                move |_, (), window, _cx| {
                    let count = Rc::clone(&count);
                    register_wallet_activity_listeners(
                        window,
                        Rc::new(move |_window, _cx| {
                            count.set(count.get() + 1);
                            stop_activity.get()
                        }),
                    );
                },
            )
            .absolute()
            .size_full();
            let content_clicks = Rc::clone(&self.content_clicks);
            div()
                .debug_selector(|| "activity-probe".to_owned())
                .relative()
                .size_full()
                .track_focus(&self.root_focus)
                .child(observer)
                .child(div().size_full().on_mouse_down(
                    MouseButton::Left,
                    move |_event, _window, _cx| {
                        content_clicks.set(content_clicks.get() + 1);
                    },
                ))
                .when(self.dialog_open, |this| {
                    this.child(
                        div()
                            .debug_selector(|| "activity-dialog".to_owned())
                            .absolute()
                            .size_full()
                            .track_focus(&self.dialog_focus)
                            .occlude()
                            .on_key_down(|_, _, cx| cx.stop_propagation())
                            .on_mouse_move(|_, _, cx| cx.stop_propagation())
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_scroll_wheel(|_, _, cx| cx.stop_propagation()),
                    )
                })
        }
    }

    #[gpui::test]
    fn wallet_activity_observer_captures_content_and_dialog_events(cx: &mut TestAppContext) {
        let count = Rc::new(Cell::new(0));
        let probe_count = Rc::clone(&count);
        let (probe, cx) = cx.add_window_view(|window, cx| {
            let wallet_window = window.window_handle();
            let key_count = Rc::clone(&probe_count);
            let keystroke_interceptor = cx.intercept_keystrokes(move |_event, window, _cx| {
                if window.window_handle() == wallet_window {
                    key_count.set(key_count.get() + 1);
                }
            });
            ActivityObserverProbe {
                count: probe_count,
                stop_activity: Rc::new(Cell::new(false)),
                content_clicks: Rc::new(Cell::new(0)),
                root_focus: cx.focus_handle(),
                dialog_focus: cx.focus_handle(),
                dialog_open: false,
                _keystroke_interceptor: keystroke_interceptor,
            }
        });
        cx.update(|window, app| {
            window.focus(&probe.read(app).root_focus);
            window.activate_window();
        });
        cx.refresh().expect("refresh test window");
        cx.run_until_parked();
        let content_position = cx
            .debug_bounds("activity-probe")
            .expect("probe bounds")
            .center();

        cx.simulate_mouse_move(content_position, None::<MouseButton>, Modifiers::none());
        cx.simulate_mouse_down(content_position, MouseButton::Left, Modifiers::none());
        cx.simulate_event(ScrollWheelEvent {
            position: content_position,
            delta: ScrollDelta::Pixels(point(px(0.0), px(-10.0))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        cx.simulate_event(KeyDownEvent {
            keystroke: Keystroke::parse("a").expect("valid test keystroke"),
            is_held: false,
        });
        assert_eq!(count.get(), 4);

        probe.update(cx, |probe, cx| {
            probe.dialog_open = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, app| {
            window.focus(&probe.read(app).dialog_focus);
        });
        let dialog_position = cx
            .debug_bounds("activity-dialog")
            .expect("dialog bounds")
            .center();

        cx.simulate_mouse_move(dialog_position, None::<MouseButton>, Modifiers::none());
        cx.simulate_mouse_down(dialog_position, MouseButton::Left, Modifiers::none());
        cx.simulate_event(ScrollWheelEvent {
            position: dialog_position,
            delta: ScrollDelta::Pixels(point(px(0.0), px(-10.0))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        cx.simulate_event(KeyDownEvent {
            keystroke: Keystroke::parse("b").expect("valid test keystroke"),
            is_held: false,
        });
        assert_eq!(count.get(), 8);
    }

    #[gpui::test]
    fn wallet_activity_observer_stops_expired_input_before_content(cx: &mut TestAppContext) {
        let count = Rc::new(Cell::new(0));
        let probe_count = Rc::clone(&count);
        let content_clicks = Rc::new(Cell::new(0));
        let probe_content_clicks = Rc::clone(&content_clicks);
        let (probe, cx) = cx.add_window_view(|_window, cx| ActivityObserverProbe {
            count: probe_count,
            stop_activity: Rc::new(Cell::new(true)),
            content_clicks: probe_content_clicks,
            root_focus: cx.focus_handle(),
            dialog_focus: cx.focus_handle(),
            dialog_open: false,
            _keystroke_interceptor: cx.intercept_keystrokes(|_, _, _| {}),
        });
        cx.update(|window, app| {
            window.focus(&probe.read(app).root_focus);
            window.activate_window();
        });
        cx.refresh().expect("refresh test window");
        cx.run_until_parked();
        let content_position = cx
            .debug_bounds("activity-probe")
            .expect("probe bounds")
            .center();

        cx.simulate_mouse_down(content_position, MouseButton::Left, Modifiers::none());

        assert_eq!(count.get(), 1);
        assert_eq!(content_clicks.get(), 0);
    }

    #[test]
    fn network_context_transition_planner_retains_only_for_tor() {
        assert_eq!(
            network_context_plan(
                Some(WalletNetworkMode::Tor),
                WalletNetworkMode::Direct,
                false,
            ),
            NetworkContextPlan::RetainActiveAndBuild,
        );
        assert_eq!(
            network_context_plan(
                Some(WalletNetworkMode::Tor),
                WalletNetworkMode::Proxy,
                false,
            ),
            NetworkContextPlan::RetainActiveAndBuild,
        );
        assert_eq!(
            network_context_plan(
                Some(WalletNetworkMode::Direct),
                WalletNetworkMode::Tor,
                true,
            ),
            NetworkContextPlan::ReuseRetained,
        );
        assert_eq!(
            network_context_plan(Some(WalletNetworkMode::Tor), WalletNetworkMode::Tor, true),
            NetworkContextPlan::ReuseActive,
        );
        assert_eq!(
            network_context_plan(None, WalletNetworkMode::Direct, true),
            NetworkContextPlan::Build,
        );
    }
}
