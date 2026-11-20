#[allow(clippy::wildcard_imports)]
use super::*;

impl HttpContext {
    /// Return the broker for submitted `eth_call` and `eth_getBalance` reads.
    #[must_use]
    pub fn rpc_broker(&self) -> Arc<RpcBroker> {
        self.broker.clone()
    }

    /// The JSON-RPC client for an endpoint with `transport`.
    #[must_use]
    pub const fn rpc_client_for(&self, transport: RpcTransport) -> &reqwest::Client {
        match transport {
            RpcTransport::Wallet => &self.rpc_client,
            RpcTransport::Direct => &self.direct_rpc_client,
        }
    }

    pub(crate) fn rpc_clients(&self) -> RpcClients {
        RpcClients::new(self.rpc_client.clone(), self.direct_rpc_client.clone())
    }

    #[must_use]
    pub const fn network_mode(&self) -> WalletNetworkMode {
        self.mode
    }

    #[must_use]
    pub const fn fail_closed(&self) -> bool {
        self.fail_closed
    }

    #[must_use]
    pub const fn network_status_label(&self) -> &'static str {
        self.mode.status_label()
    }

    #[must_use]
    pub fn network_status_detail(&self) -> String {
        self.network_health().detail.to_string()
    }

    #[must_use]
    pub fn network_health(&self) -> WalletNetworkHealth {
        match self.mode {
            WalletNetworkMode::Tor => self.tor_network_health(),
            WalletNetworkMode::Proxy | WalletNetworkMode::Direct => WalletNetworkHealth::new(
                self.mode,
                WalletNetworkHealthState::Ready,
                self.configured_network_status_detail(),
            ),
        }
    }

    #[must_use]
    pub fn tor_bridge_activity_snapshot(&self) -> Option<TorBridgeActivitySnapshot> {
        self.socks_bridge
            .as_ref()
            .and_then(|bridge| bridge.activity_snapshot())
    }

    fn configured_network_status_detail(&self) -> String {
        match self.mode {
            WalletNetworkMode::Tor => match self.proxy_url.as_ref() {
                Some(proxy) => format!(
                    "Ready. HTTP/RPC session #{} is routed through {proxy}",
                    self.tor_session_generation(),
                    proxy = redact_url_for_display(proxy)
                ),
                None => "HTTP bridge is unavailable".to_string(),
            },
            WalletNetworkMode::Proxy => match self.user_proxy_url.as_ref() {
                Some(proxy) => format!(
                    "HTTP is routed through {proxy}",
                    proxy = redact_url_for_display(proxy)
                ),
                None => "Missing proxy URL".to_string(),
            },
            WalletNetworkMode::Direct => {
                "Not Tor-protected; outbound requests use the network directly".to_string()
            }
        }
    }

    fn tor_network_health(&self) -> WalletNetworkHealth {
        let Some(arti_client) = self.arti_client() else {
            return WalletNetworkHealth::with_cause(
                WalletNetworkMode::Tor,
                WalletNetworkHealthState::Degraded,
                "Degraded. Tor client is unavailable",
                WalletNetworkHealthCause::TorBootstrap,
            );
        };

        self.tor_network_health_for_status(&arti_client.bootstrap_status())
    }

    fn tor_network_health_for_status(&self, status: &BootstrapStatus) -> WalletNetworkHealth {
        if status.ready_for_traffic() {
            if let Some(activity) = self.socks_bridge.as_ref() {
                let report = activity.runtime_health_report();
                match report.health {
                    TorRuntimeHealth::Slow => {
                        return WalletNetworkHealth::with_cause(
                            WalletNetworkMode::Tor,
                            WalletNetworkHealthState::Degraded,
                            format_tor_runtime_health_detail("slow", report),
                            WalletNetworkHealthCause::TorRuntimeSlow,
                        );
                    }
                    TorRuntimeHealth::Unreliable => {
                        return WalletNetworkHealth::with_cause(
                            WalletNetworkMode::Tor,
                            WalletNetworkHealthState::Degraded,
                            format_tor_runtime_health_detail("unreliable", report),
                            WalletNetworkHealthCause::TorRuntimeUnreliable,
                        );
                    }
                    TorRuntimeHealth::Ready => {}
                }
            }
            return WalletNetworkHealth::new(
                WalletNetworkMode::Tor,
                WalletNetworkHealthState::Ready,
                self.configured_network_status_detail(),
            );
        }

        let (state, prefix) = if status.blocked().is_some() {
            (WalletNetworkHealthState::Degraded, "Degraded")
        } else {
            (WalletNetworkHealthState::Reconnecting, "Reconnecting")
        };

        WalletNetworkHealth::with_cause(
            WalletNetworkMode::Tor,
            state,
            format!("{prefix}. {status}"),
            WalletNetworkHealthCause::TorBootstrap,
        )
    }

    #[must_use]
    pub fn arti_client(&self) -> Option<WalletTorClient> {
        if let Some(socks_bridge) = self.socks_bridge.as_ref() {
            match socks_bridge.active_client() {
                Ok(client) => return Some(client),
                Err(error) => {
                    tracing::warn!(target: LOG_TARGET,%error, "failed to read active Tor session client");
                }
            }
        }
        self.arti_client.clone()
    }

    #[must_use]
    pub fn arti_client_provider(&self) -> Option<WalletTorClientProvider> {
        if let Some(socks_bridge) = self.socks_bridge.as_ref() {
            return Some(ArtiSocksBridge::session_client_provider(
                &socks_bridge.session,
            ));
        }

        let arti_client = self.arti_client.clone()?;
        Some(Arc::new(move || Some(arti_client.clone())))
    }

    pub fn start_new_tor_session(&self) -> Result<u64> {
        if self.mode != WalletNetworkMode::Tor {
            return Err(eyre!("new Tor session is only available in Tor mode"));
        }
        let socks_bridge = self
            .socks_bridge
            .as_ref()
            .ok_or_else(|| eyre!("new Tor session requires the internal SOCKS bridge"))?;
        // Held across both steps so a concurrently opened operation bridge joins this session.
        let mut operation_bridges = self.lock_operation_bridges()?;
        let generation = socks_bridge.new_isolated_session()?;
        operation_bridges.retain(|bridge| bridge.strong_count() > 0);
        for bridge in operation_bridges.iter().filter_map(Weak::upgrade) {
            bridge.join_session(generation)?;
        }
        drop(operation_bridges);
        self.gateway_pool.reset();
        Ok(generation)
    }

    fn lock_operation_bridges(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Vec<Weak<ArtiSocksBridge>>>> {
        self.operation_bridges
            .lock()
            .map_err(|_| eyre!("operation Tor bridge list lock is poisoned"))
    }

    /// Build an HTTP client for the traffic of one wallet operation, such as one swap.
    ///
    /// In built-in Tor mode the operation gets its own loopback SOCKS bridge over an
    /// `isolated_client()` handle of the shared Arti instance. Arti gives each `TorClient`
    /// handle its own `IsolationToken`, and streams with different tokens never share a
    /// circuit. Sync, RPC, broadcaster, and other operations' traffic use other handles, so
    /// none of it shares circuits with this operation. The clients use the same proxy-only
    /// builder as the shared clients, so a failed bridge fails the request rather than
    /// bypassing Tor. Each client's connector holds the bridge, which stops when the last clone
    /// of either client is dropped. [`Self::start_new_tor_session`] moves every live bridge onto
    /// a fresh isolated client.
    ///
    /// Proxy and direct modes can't isolate one operation's traffic. They return the shared
    /// clients and report isolation as unavailable so that callers can disclose it.
    pub async fn operation_http_client(&self) -> Result<OperationHttpClient> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(open) = &self.operation_clients_for_tests {
            return open();
        }
        match self.mode {
            WalletNetworkMode::Tor => {
                if self.socks_bridge.is_none() {
                    return Err(eyre!(
                        "operation network isolation requires the internal SOCKS bridge"
                    ));
                }
                let arti_client = self
                    .arti_client()
                    .ok_or_else(|| eyre!("Tor client is unavailable"))?;
                let bridge = Arc::new(
                    ArtiSocksBridge::start(
                        arti_client.isolated_client(),
                        self.tor_session_generation(),
                    )
                    .await
                    .wrap_err("start operation Arti SOCKS bridge")?,
                );
                {
                    let mut operation_bridges = self.lock_operation_bridges()?;
                    // A session started while the bridge opened moves it on at once.
                    let generation = self.tor_session_generation();
                    if bridge.session_generation() != generation {
                        bridge.join_session(generation)?;
                    }
                    operation_bridges.retain(|bridge| bridge.strong_count() > 0);
                    operation_bridges.push(Arc::downgrade(&bridge));
                }
                let proxy_url = Url::parse(&format!("socks5h://{}", bridge.local_addr()))
                    .wrap_err("build operation Arti SOCKS proxy URL")?;
                let builder = || -> Result<reqwest::ClientBuilder> {
                    let bridge = Arc::clone(&bridge);
                    // A no-op connector layer whose services keep the bridge running.
                    let hold_bridge = tower::util::MapErrLayer::new(
                        move |error: Box<dyn std::error::Error + Send + Sync>| {
                            let _ = &bridge;
                            error
                        },
                    );
                    Ok(
                        wallet_reqwest_client_builder(WalletNetworkMode::Tor, Some(&proxy_url))?
                            .connector_layer(hold_bridge),
                    )
                };
                Ok(OperationHttpClient {
                    client: builder()?.build().wrap_err("build operation HTTP client")?,
                    rpc_client: builder()?
                        .timeout(rpc_request_timeout(WalletNetworkMode::Tor))
                        .build()
                        .wrap_err("build operation RPC HTTP client")?,
                    isolation: OperationNetworkIsolation::Dedicated,
                    tor_client: Some(ArtiSocksBridge::session_client_provider(&bridge.session)),
                    bridge: Some(Arc::downgrade(&bridge)),
                    #[cfg(test)]
                    reisolations: Arc::default(),
                })
            }
            WalletNetworkMode::Proxy | WalletNetworkMode::Direct => Ok(OperationHttpClient {
                client: self.client.clone(),
                rpc_client: self.rpc_client.clone(),
                isolation: OperationNetworkIsolation::Unavailable(self.mode),
                tor_client: None,
                bridge: None,
                #[cfg(test)]
                reisolations: Arc::default(),
            }),
        }
    }

    #[must_use]
    pub fn gateway_pool(&self) -> GatewayPool {
        self.gateway_pool.clone()
    }

    #[must_use]
    pub fn tor_session_generation(&self) -> u64 {
        self.socks_bridge
            .as_ref()
            .map_or(0, |socks_bridge| socks_bridge.session_generation())
    }

    #[must_use]
    pub fn arti_state_dir(&self) -> Option<&Path> {
        self.arti_state_dir.as_deref()
    }

    #[must_use]
    pub fn arti_cache_dir(&self) -> Option<&Path> {
        self.arti_cache_dir.as_deref()
    }

    #[must_use]
    pub const fn has_internal_socks_bridge(&self) -> bool {
        self.socks_bridge.is_some()
    }

    #[cfg(test)]
    pub(crate) fn direct_for_tests() -> Self {
        Self::direct_for_tests_with_candidate_order(|_| {})
    }

    /// Like `direct_for_tests`, with `candidate_order` arranging the broker's endpoint ties.
    #[cfg(test)]
    pub(crate) fn direct_for_tests_with_candidate_order(candidate_order: fn(&mut [usize])) -> Self {
        let rpc_client = reqwest::Client::new();
        Self {
            client: reqwest::Client::new(),
            rpc_client: rpc_client.clone(),
            direct_rpc_client: rpc_client.clone(),
            broker: RpcBroker::spawn_with_candidate_order(
                rpc_client.clone(),
                rpc_client,
                candidate_order,
            )
            .expect("HttpContext test constructor requires an active Tokio runtime"),
            gateway_pool: GatewayPool::new(),
            proxy_url: None,
            user_proxy_url: None,
            mode: WalletNetworkMode::Direct,
            arti_client: None,
            arti_state_dir: None,
            arti_cache_dir: None,
            socks_bridge: None,
            operation_bridges: Arc::default(),
            fail_closed: false,
            operation_clients_for_tests: None,
        }
    }

    /// # Panics
    ///
    /// Panics without an active Tokio runtime.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_rpc_client_for_tests(rpc_client: reqwest::Client, mode: WalletNetworkMode) -> Self {
        Self::with_rpc_clients_for_tests(rpc_client.clone(), rpc_client, mode)
    }

    /// Like [`Self::with_rpc_client_for_tests`], with a separate client for direct endpoints.
    ///
    /// # Panics
    ///
    /// Panics without an active Tokio runtime.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn with_rpc_clients_for_tests(
        rpc_client: reqwest::Client,
        direct_rpc_client: reqwest::Client,
        mode: WalletNetworkMode,
    ) -> Self {
        #[cfg(test)]
        let broker = RpcBroker::spawn_with_candidate_order(
            rpc_client.clone(),
            direct_rpc_client.clone(),
            |_| {},
        );
        // The fixed-order constructor is crate-test-only; downstream tests use the default order.
        #[cfg(not(test))]
        let broker = RpcBroker::spawn(rpc_client.clone(), direct_rpc_client.clone());
        let broker = broker.expect("HttpContext test constructor requires an active Tokio runtime");
        Self {
            client: reqwest::Client::new(),
            direct_rpc_client,
            rpc_client,
            broker,
            gateway_pool: GatewayPool::new(),
            proxy_url: None,
            user_proxy_url: None,
            mode,
            arti_client: None,
            arti_state_dir: None,
            arti_cache_dir: None,
            socks_bridge: None,
            operation_bridges: Arc::default(),
            fail_closed: mode != WalletNetworkMode::Direct,
            operation_clients_for_tests: None,
        }
    }

    /// Opens every operation client with `open` instead of from the network mode.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_operation_clients_for_tests(
        mut self,
        open: impl Fn() -> Result<OperationHttpClient> + Send + Sync + 'static,
    ) -> Self {
        self.operation_clients_for_tests = Some(Arc::new(open));
        self
    }
}
