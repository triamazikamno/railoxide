#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletStartupRoot {
    pub(in crate::root) fn root_replacement_is_allowed(&self, cx: &App) -> bool {
        self.wallet_root
            .as_ref()
            .is_none_or(|root| root.read(cx).root_replacement_is_allowed())
    }

    pub(in crate::root) fn retry_startup_with_network_context(
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
        // An active root means setup already finished on this data folder.
        if self.wallet_root.is_none() && !self.first_start_admits_startup(&vault_store, window, cx)
        {
            return;
        }
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
            .map(|root| root.update(cx, crate::root::WalletRoot::begin_root_replacement_shutdown));
        let (cleanup, outgoing_http) =
            replacement.map_or((None, None), |(cleanup, http, tracker)| {
                self.public_transaction_tracker = tracker;
                (Some(cleanup), Some(http))
            });
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

    pub(super) fn reset_settings_and_retry(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
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

    pub(super) fn quit_and_reset_tor_state(&mut self, cx: &mut Context<'_, Self>) {
        if !self.tor_bootstrap_recovery_available {
            return;
        }
        match request_tor_state_reset(&self.options.db_path) {
            Ok(marker_path) => {
                tracing::warn!(target: LOG_TARGET,
                    marker_path = %marker_path.display(),
                    "requested Tor state reset on next wallet startup; quitting wallet"
                );
                cx.quit();
            }
            Err(error) => {
                tracing::warn!(target: LOG_TARGET, %error, "failed to request Tor state reset during startup");
                self.tor_reset_error = Some(Arc::from(format_report_chain(&error)));
                cx.notify();
            }
        }
    }

    pub(super) fn open_settings_data_folder(&mut self, cx: &mut Context<'_, Self>) {
        self.data_folder_open_error = None;
        cx.notify();
        let root = cx.weak_entity();
        open_data_folder(
            data_folder_path(&self.options.db_path),
            cx,
            move |error, cx| {
                let _ = root.update(cx, |root, cx| {
                    root.data_folder_open_error = Some(error);
                    cx.notify();
                });
            },
        );
    }
}
