#[allow(clippy::wildcard_imports)]
use super::*;

impl OperationHttpClient {
    #[must_use]
    pub const fn isolation(&self) -> OperationNetworkIsolation {
        self.isolation
    }

    pub(crate) const fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub(crate) const fn rpc_client(&self) -> &reqwest::Client {
        &self.rpc_client
    }

    /// Yields the client of the operation's own Tor bridge, which a new Tor session
    /// re-isolates. `None` for the shared clients of proxy and direct modes.
    #[must_use]
    pub fn tor_client_provider(&self) -> Option<WalletTorClientProvider> {
        self.tor_client.clone()
    }

    /// Gives the operation's own Tor bridge new circuits in the current session; a later
    /// [`HttpContext::start_new_tor_session`] still moves it on. Without a bridge it does nothing.
    pub fn reisolate(&self) -> Result<()> {
        #[cfg(test)]
        self.reisolations.fetch_add(1, Ordering::SeqCst);
        if let Some(bridge) = self.bridge.as_ref().and_then(Weak::upgrade) {
            bridge.reisolate()?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn reisolations_for_tests(&self) -> usize {
        self.reisolations.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(super) fn bridge(&self) -> Option<Arc<ArtiSocksBridge>> {
        self.bridge.as_ref().and_then(Weak::upgrade)
    }

    /// Uses `client` for both general HTTP and JSON-RPC.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn for_tests(client: reqwest::Client, isolation: OperationNetworkIsolation) -> Self {
        Self {
            rpc_client: client.clone(),
            client,
            isolation,
            tor_client: None,
            bridge: None,
            #[cfg(test)]
            reisolations: Arc::default(),
        }
    }

    /// Reports `provider` as this client's Tor provider.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_tor_client_provider_for_tests(
        mut self,
        provider: WalletTorClientProvider,
    ) -> Self {
        self.tor_client = Some(provider);
        self
    }

    /// A direct client of its own, leaked so that test closures and spawned futures can borrow
    /// it.
    #[cfg(test)]
    pub(crate) fn direct_for_tests() -> &'static Self {
        Box::leak(Box::new(Self::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        )))
    }
}

impl fmt::Debug for OperationHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperationHttpClient")
            .field("isolation", &self.isolation)
            .finish_non_exhaustive()
    }
}

impl OperationHttpClientSlot {
    #[must_use]
    pub fn new(http: HttpContext) -> Self {
        Self {
            http,
            client: Arc::default(),
        }
    }

    /// Opens the client on the first call; a failed open leaves the slot empty for a later retry.
    pub async fn client(&self) -> Result<OperationHttpClient> {
        self.client
            .get_or_try_init(|| self.http.operation_http_client())
            .await
            .cloned()
    }

    /// A slot already holding `client`.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn for_tests(http: HttpContext, client: OperationHttpClient) -> Self {
        Self {
            http,
            client: Arc::new(tokio::sync::OnceCell::from(client)),
        }
    }
}

impl fmt::Debug for OperationHttpClientSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperationHttpClientSlot")
            .finish_non_exhaustive()
    }
}
