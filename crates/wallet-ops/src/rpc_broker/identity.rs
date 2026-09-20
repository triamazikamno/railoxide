use alloy::providers::{Provider, ProviderBuilder};
use poi::SensitiveUrl;
use std::time::Duration;

use super::{RpcBrokerError, RpcChainRoute};

impl RpcChainRoute {
    fn endpoint_verification(&self, endpoint: &SensitiveUrl) -> &tokio::sync::OnceCell<()> {
        &self
            .verified_identities
            .iter()
            .find(|(url, _)| url == endpoint)
            .expect("endpoint belongs to this route")
            .1
    }

    pub(super) fn endpoint_identity_verified(&self, endpoint: &SensitiveUrl) -> bool {
        !self.requires_identity_verification() || self.endpoint_verification(endpoint).initialized()
    }

    /// Successful checks belong to this immutable route and its clones. Failed checks
    /// remain retryable; replacing the configuration starts with no verified endpoints.
    pub(super) async fn verify_endpoint_identity(
        &self,
        client: &reqwest::Client,
        endpoint: &SensitiveUrl,
        timeout: Duration,
    ) -> Result<(), RpcBrokerError> {
        if !self.requires_identity_verification() {
            return Ok(());
        }
        tokio::time::timeout(
            timeout,
            self.endpoint_verification(endpoint)
                .get_or_try_init(|| async {
                    let provider = ProviderBuilder::new()
                        .connect_reqwest(client.clone(), endpoint.expose_url().clone());
                    let id = provider
                        .get_chain_id()
                        .await
                        .map_err(|_| RpcBrokerError::Transport)?;
                    if id != self.chain_id() {
                        return Err(RpcBrokerError::InvalidResponse);
                    }
                    Ok(())
                }),
        )
        .await
        .map_err(|_| RpcBrokerError::Timeout)?
        .copied()
    }

    pub(crate) async fn verify_identity(
        &self,
        client: &reqwest::Client,
    ) -> Result<Self, RpcBrokerError> {
        let mut endpoints = Vec::new();
        for endpoint in self.endpoints() {
            if self
                .verify_endpoint_identity(client, endpoint, Duration::from_secs(10))
                .await
                .is_ok()
            {
                endpoints.push(endpoint.clone());
            }
        }
        if endpoints.is_empty() {
            return Err(RpcBrokerError::NoEndpoint {
                chain_id: self.chain_id(),
            });
        }
        let mut route = Self::new(self.chain_id(), endpoints);
        if let Some(multicall) = self.multicall() {
            route = route.with_multicall(multicall);
        }
        // This snapshot is used only by the operation that awaited verification.
        Ok(route)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U64;
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    #[tokio::test]
    async fn endpoint_identity_does_not_transfer_to_replacement_configuration() {
        let returned_id = Arc::new(AtomicU64::new(1));
        let observed = returned_id.clone();
        let (endpoint, server) = super::super::tests::spawn_rpc_mock(
            Arc::new(move |request| {
                assert_eq!(request["method"], "eth_chainId");
                json!({"jsonrpc":"2.0", "id":request["id"], "result": U64::from(observed.load(Ordering::SeqCst))})
            }), Arc::default(), Arc::default(),
        ).await;
        let http = crate::HttpContext::direct_for_tests();
        let route =
            RpcChainRoute::new(999_999, vec![endpoint.clone()]).with_identity_verification();
        assert!(route.verify_identity(&http.rpc_client).await.is_err());
        returned_id.store(999_999, Ordering::SeqCst);
        let equivalent =
            RpcChainRoute::new(999_999, route.endpoint_urls()).with_identity_verification();
        let fingerprint = |route: &RpcChainRoute| {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            route.hash(&mut hasher);
            hasher.finish()
        };
        let before_verification = fingerprint(&route);
        assert_eq!(
            route
                .verify_identity(&http.rpc_client)
                .await
                .unwrap()
                .endpoint_urls(),
            vec![endpoint]
        );
        let (replacement, replacement_server) = super::super::tests::spawn_rpc_mock(
            Arc::new(|request| json!({"jsonrpc":"2.0", "id":request["id"], "result":"0x1"})),
            Arc::default(),
            Arc::default(),
        )
        .await;
        let replacement =
            RpcChainRoute::new(999_999, vec![replacement]).with_identity_verification();
        assert!(replacement.verify_identity(&http.rpc_client).await.is_err());
        // Populating a cache must not change broker grouping or authority equality.
        assert_eq!(route, equivalent);
        assert_eq!(fingerprint(&route), before_verification);
        assert_eq!(fingerprint(&equivalent), before_verification);
        // A rebuilt configuration must verify even a formerly correct endpoint.
        returned_id.store(1, Ordering::SeqCst);
        assert!(
            route
                .clone()
                .verify_identity(&http.rpc_client)
                .await
                .is_ok()
        );
        let rebuilt =
            RpcChainRoute::new(999_999, route.endpoint_urls()).with_identity_verification();
        assert!(rebuilt.verify_identity(&http.rpc_client).await.is_err());
        server.abort();
        replacement_server.abort();
    }
}
