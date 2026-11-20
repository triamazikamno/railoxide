#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletStartupRoot {
    pub(in crate::root) fn new(
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
                                root.handle_wallet_activity(
                                    crate::root::auto_lock::WalletActivitySource::Desktop,
                                    event_window,
                                    cx,
                                )
                            })
                        })
                    })
                    .unwrap_or(false);
                if locked {
                    cx.stop_propagation();
                }
            });
        let mut root = Self {
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
            public_transaction_tracker: wallet_ops::PublicTransactionTracker::default(),
            maintenance_controller,
            startup_generation: 1,
            retained_tor_http: None,
            tor_bootstrap_recovery_available: false,
            tor_reset_error: None,
            data_folder_open_error: None,
            setup: None,
            _activity_keystroke_interceptor: activity_keystroke_interceptor,
        };
        if let Some(vault_store) = vault_store
            && root.first_start_admits_startup(&vault_store, window, cx)
        {
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

    pub(super) fn spawn_startup_tasks(
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
                    tracing::debug!(target: LOG_TARGET, generation, "ignoring stale wallet startup result");
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
        let private_chain_ids: Vec<u64> = ready
            .effective_chain_configs
            .railgun_chains()
            .map(|chain| chain.chain_id)
            .collect();
        let anchor_effective_chains = ready.effective_chain_configs.clone();
        let anchor_token_registry = ready.effective_token_registry.clone();
        let public_broadcaster_anchor_refresh = spawn_token_anchor_refresh_worker(
            &self.runtime,
            Arc::clone(&public_broadcaster_anchor_cache),
            private_chain_ids.clone(),
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
                    .map(|token| (chain.chain_id, token))
            })
            .collect();
        let monitor = cx.new(|cx| {
            broadcaster_monitor_gpui::BroadcasterMonitorPane::new(
                self.monitor_state.clone(),
                event_rx,
                &private_chain_ids,
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
                ready,
                WalletRootDependencies {
                    options: self.options.clone(),
                    public_transaction_tracker: self.public_transaction_tracker.clone(),
                    runtime: self.runtime.clone(),
                    monitor_state,
                    monitor_event_rx: wallet_monitor_event_rx,
                    monitor,
                    public_broadcaster_anchor_cache,
                    public_broadcaster_anchor_refresh,
                    logs,
                    startup_root: &startup_root,
                    maintenance_controller: &maintenance_controller,
                },
                window,
                cx,
            )
        });
        root.update(cx, |root, _| {
            root.install_chain_background();
        });
        self.error = None;
        self.wallet_root = Some(root);
        if ready_is_tor {
            self.retained_tor_http = None;
        }
        cx.notify();
    }

    pub(super) fn retain_tor_context(&mut self, http: HttpContext) {
        if !is_retained_tor_context(&http) {
            return;
        }
        let generation = http.tor_session_generation();
        tracing::debug!(target: LOG_TARGET, generation, "retaining warm Tor network context");
        let _ = self.retained_tor_http.replace(http);
    }

    pub(super) fn reuse_retained_tor_context(&self) -> Option<HttpContext> {
        let http = self.retained_tor_http.as_ref()?.clone();
        let generation = http.tor_session_generation();
        tracing::debug!(target: LOG_TARGET, generation, "reusing retained Tor network context");
        Some(http)
    }

    pub(super) fn fail_startup(&mut self, message: String, cx: &mut Context<'_, Self>) {
        tracing::error!(target: LOG_TARGET, error = %message, "wallet startup failed");
        self.error = Some(Arc::from(message));
        cx.notify();
    }

    pub(super) fn startup_vault_store(&mut self) -> Result<Arc<DesktopVaultStore>, String> {
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

    pub(in crate::root) fn retry_startup(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if !self.maintenance_controller.read(cx).is_idle() {
            return;
        }
        self.retry_startup_with_network_context(None, window, cx);
    }
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
        chain_ids: effective_chain_configs
            .railgun_chains()
            .map(|chain| chain.chain_id)
            .collect(),
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
        backup_peers: settings
            .waku
            .backup_peers
            .clone()
            .unwrap_or_else(default_waku_backup_peers)
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

    tracing::info!(target: LOG_TARGET,
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
            tracing::info!(target: LOG_TARGET,
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
        tracing::warn!(target: LOG_TARGET,
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
