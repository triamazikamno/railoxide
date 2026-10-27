use alloy::eips::{BlockId, BlockNumHash, BlockNumberOrTag};
use alloy::network::primitives::HeaderResponse as _;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider as _;
use eyre::{Result, eyre};

use super::executor_discovery::WRONG_CHAIN;
use super::executor_observation::ObservationEndpoints;
use crate::HttpContext;
use crate::settings::EffectiveChainConfig;

/// Reads `address`'s native balance at the chain's latest block over an operation client opened
/// for this call alone, which in built-in Tor mode is a route of its own. The chain is verified
/// before the address is sent, and a failure after sending isn't retried on another endpoint.
///
/// # Errors
///
/// Returns an error when no operation route opens, no endpoint serves the chain, or the balance
/// read fails.
pub async fn check_native_balance(
    http: &HttpContext,
    chain: &EffectiveChainConfig,
    address: Address,
) -> Result<(U256, BlockNumHash)> {
    let client = http.operation_http_client().await?;
    // Health is the broker's, so an endpoint this check finds failing sits out the next.
    let endpoints =
        ObservationEndpoints::new(chain, http, crate::RpcTrafficClass::Sensitive, &client)?;
    let mut attempts = endpoints.attempts();
    while let Some(provider) = attempts.next().await {
        let head = async {
            // Validate the chain before disclosing the address.
            if provider.provider.get_chain_id().await? != chain.chain_id {
                return Err(eyre!(WRONG_CHAIN));
            }
            provider
                .provider
                .get_block_by_number(BlockNumberOrTag::Latest)
                .await?
                .ok_or_else(|| eyre!("latest block is unavailable"))
        }
        .await;
        let block = match head {
            Ok(head) => head.header.num_hash(),
            Err(error) => {
                endpoints.failed(&provider, &error);
                continue;
            }
        };
        // The address has been sent, so a failure ends this check.
        return match provider
            .provider
            .get_balance(address)
            .block_id(BlockId::hash_canonical(block.hash))
            .await
            .map_err(eyre::Report::from)
        {
            Ok(balance) => {
                endpoints.succeeded(&provider);
                Ok((balance, block))
            }
            Err(error) => {
                endpoints.failed(&provider, &error);
                Err(error)
            }
        };
    }
    Err(eyre!("native balance check is unavailable"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use alloy::primitives::B256;
    use serde_json::{Value, json};

    use super::*;
    use crate::rpc_broker::tests::{
        RecordedRequests, spawn_recording_rpc_mock, tagged_reqwest_client,
    };

    fn chain(endpoints: Vec<url::Url>) -> EffectiveChainConfig {
        let mut chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        chain.rpc_route = crate::RpcChainRoute::new(1, endpoints);
        chain
    }

    /// An endpoint serving `chain_id` whose balance reads return `balance`, or fail without one.
    async fn endpoint(
        chain_id: &'static str,
        balance: Option<&'static str>,
    ) -> (url::Url, RecordedRequests, tokio::task::JoinHandle<()>) {
        let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
        block.header.inner.number = 10;
        block.header.hash = B256::repeat_byte(10);
        let block = serde_json::to_value(block).unwrap();
        spawn_recording_rpc_mock(Arc::new(move |request: Value| {
            let mut response = match (request["method"].as_str().unwrap(), balance) {
                ("eth_chainId", _) => json!({"result": chain_id}),
                ("eth_getBlockByNumber", _) => json!({"result": block}),
                ("eth_getBalance", Some(balance)) => json!({"result": balance}),
                ("eth_getBalance", None) => {
                    json!({"error": {"code": -32005, "message": "limit exceeded"}})
                }
                (method, _) => panic!("unexpected RPC method {method}"),
            };
            response["jsonrpc"] = json!("2.0");
            response["id"] = request["id"].clone();
            response
        }))
        .await
    }

    fn methods(requests: &RecordedRequests) -> Vec<String> {
        requests
            .lock()
            .unwrap()
            .iter()
            .map(|(_, request)| request["method"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn balance_check_verifies_the_chain_first_never_fails_over_and_opens_a_route_per_call() {
        let address = Address::repeat_byte(7);
        let (wrong, wrong_requests, wrong_task) = endpoint("0x2", Some("0x2a")).await;
        let (failing, failing_requests, failing_task) = endpoint("0x1", None).await;
        let (serving, serving_requests, serving_task) = endpoint("0x1", Some("0x2a")).await;
        let route_clients = ["route-0", "route-1", "route-2"].map(tagged_reqwest_client);
        let opened = Arc::new(AtomicUsize::new(0));
        let http = HttpContext::with_rpc_client_for_tests(
            tagged_reqwest_client("shared"),
            crate::WalletNetworkMode::Tor,
        )
        .with_operation_clients_for_tests({
            let opened = opened.clone();
            move || {
                let client = route_clients[opened.fetch_add(1, Ordering::Relaxed)].clone();
                Ok(crate::OperationHttpClient::for_tests(
                    client,
                    crate::OperationNetworkIsolation::Dedicated,
                ))
            }
        });

        // Tried in configured order: the wrong chain never sees the address, and the balance
        // read that failed after the address was sent isn't retried on the serving endpoint.
        let checked = check_native_balance(
            &http,
            &chain(vec![wrong.clone(), failing, serving.clone()]),
            address,
        )
        .await;
        assert!(checked.is_err());
        assert_eq!(methods(&wrong_requests), ["eth_chainId"]);
        assert_eq!(
            methods(&failing_requests),
            ["eth_chainId", "eth_getBlockByNumber", "eth_getBalance"]
        );
        assert!(serving_requests.lock().unwrap().is_empty());

        for _ in 0..2 {
            let (balance, block) =
                check_native_balance(&http, &chain(vec![wrong.clone(), serving.clone()]), address)
                    .await
                    .unwrap();
            assert_eq!(balance, U256::from(42));
            assert_eq!(block, BlockNumHash::new(10, B256::repeat_byte(10)));
        }
        assert!(!methods(&wrong_requests).contains(&"eth_getBalance".to_owned()));
        assert_eq!(opened.load(Ordering::Relaxed), 3);
        for requests in [&wrong_requests, &failing_requests, &serving_requests] {
            assert!(
                requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(route, _)| route.as_deref().unwrap().starts_with("route-")),
                "the shared client sends no check"
            );
        }
        // Each call sent the address over its own route, pinned to the head it verified.
        let serving_requests = serving_requests.lock().unwrap();
        let balance_reads = serving_requests
            .iter()
            .filter(|(_, request)| request["method"] == "eth_getBalance")
            .collect::<Vec<_>>();
        assert_eq!(
            balance_reads
                .iter()
                .map(|(route, _)| route.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["route-1", "route-2"]
        );
        for (_, request) in balance_reads {
            assert_eq!(
                request["params"],
                json!([address, {"blockHash": B256::repeat_byte(10), "requireCanonical": true}])
            );
        }
        drop(serving_requests);
        for task in [wrong_task, failing_task, serving_task] {
            task.abort();
        }
    }

    #[tokio::test]
    async fn balance_check_fails_without_the_shared_route_when_tor_cannot_isolate_it() {
        let (serving, requests, task) = endpoint("0x1", Some("0x2a")).await;
        let http = HttpContext::with_rpc_client_for_tests(
            tagged_reqwest_client("shared"),
            crate::WalletNetworkMode::Tor,
        )
        .with_operation_clients_for_tests(|| Err(eyre!("synthetic open failure")));

        let checked =
            check_native_balance(&http, &chain(vec![serving]), Address::repeat_byte(7)).await;
        assert!(checked.is_err());
        assert!(requests.lock().unwrap().is_empty());
        task.abort();
    }
}
