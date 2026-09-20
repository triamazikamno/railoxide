//! Token review metadata uses the requesting origin's admitted reads.
use crate::rpc_broker::RpcBrokerViewCalls;
use crate::settings::CustomTokenSettings;
use crate::{DappRpcReadClient, RpcBrokerError, SensitiveUrl};
use alloy::{
    primitives::{Address, Bytes, U256},
    providers::{MULTICALL3_ADDRESS, Provider, ProviderBuilder},
    sol_types::SolCall,
};

/// Alloy has no add-chain request type. Read only the required metadata here;
/// the original request, including extension fields, remains on the approval.
pub(super) fn proposed_chain(
    chain_id: u64,
    raw: &serde_json::Value,
) -> Result<crate::settings::CustomChainSettings, &'static str> {
    use crate::settings::{ChainMutation, CustomChainSettings, NativeCurrency, WalletSettings};
    // Persisted NativeCurrency rejects unknown settings fields. The Ethereum adapter
    // must accept currency extension fields as well as top-level extensions.
    #[derive(serde::Deserialize)]
    struct RequestedCurrency {
        name: String,
        symbol: String,
        decimals: u8,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Metadata {
        chain_name: String,
        native_currency: RequestedCurrency,
        rpc_urls: Vec<String>,
        #[serde(default)]
        block_explorer_urls: Vec<String>,
    }
    let value = raw
        .as_array()
        .and_then(|values| values.first())
        .ok_or("Missing chain metadata")?;
    let metadata: Metadata =
        serde_json::from_value(value.clone()).map_err(|_| "Invalid chain metadata")?;
    // This is the wallet's dapp-add policy, separate from manual Settings URLs.
    for endpoint in &metadata.rpc_urls {
        if endpoint.len() > 4096
            || !url::Url::parse(endpoint).is_ok_and(|url| url.scheme() == "https")
        {
            return Err("Dapp-proposed endpoints must use HTTPS");
        }
    }
    if metadata.block_explorer_urls.len() > crate::settings::MAX_CHAIN_ENDPOINTS {
        return Err("Too many explorer URLs");
    }
    let definition = CustomChainSettings {
        name: metadata.chain_name,
        native_currency: NativeCurrency {
            name: metadata.native_currency.name,
            symbol: metadata.native_currency.symbol,
            decimals: metadata.native_currency.decimals,
        },
        rpc_endpoints: metadata.rpc_urls,
        explorer_urls: metadata.block_explorer_urls,
        enabled: true,
        contracts: crate::settings::ChainContractSettings::default(),
        finality_depth: None,
        gas: crate::settings::ChainGasSettings::default(),
    };
    ChainMutation::Add {
        chain_id,
        definition: definition.clone(),
    }
    .prepare(&WalletSettings::default())
    .map_err(|_| "Invalid chain metadata")?;
    Ok(definition)
}

/// Called only after the bounded native approval enters its executing state.
/// Each endpoint must agree; a failed endpoint never leaves a partial definition.
/// Returns the common Multicall address only when its optional probe succeeds.
pub async fn verify_proposed_chain(
    http: &crate::HttpContext,
    control: &crate::dapp_request::DappRequestControl,
    chain_id: u64,
    definition: &crate::settings::CustomChainSettings,
) -> Result<Option<Address>, RpcBrokerError> {
    control.ensure_current()?;
    let endpoints = definition
        .rpc_endpoints
        .iter()
        .map(|endpoint| {
            let url = url::Url::parse(endpoint).map_err(|_| RpcBrokerError::InvalidResponse)?;
            if url.scheme() != "https" {
                return Err(RpcBrokerError::OriginRejected);
            }
            Ok(SensitiveUrl::from(url))
        })
        .collect::<Result<Vec<_>, _>>()?;
    verify_chain_endpoints(http, control, chain_id, endpoints).await
}

async fn verify_chain_endpoints(
    http: &crate::HttpContext,
    control: &crate::dapp_request::DappRequestControl,
    chain_id: u64,
    endpoints: Vec<SensitiveUrl>,
) -> Result<Option<Address>, RpcBrokerError> {
    if endpoints.is_empty() {
        return Err(RpcBrokerError::NoEndpoint { chain_id });
    }
    for endpoint in &endpoints {
        control.ensure_current()?;
        let route = crate::RpcChainRoute::new(chain_id, vec![endpoint.clone()])
            .with_identity_verification();
        tokio::select! {
            biased;
            () = control.cancelled() => return Err(RpcBrokerError::OriginRejected),
            result = route.verify_identity(&http.rpc_client) => { result?; }
        }
        control.ensure_current()?;
    }
    probe_common_multicall(http, control, chain_id, &endpoints).await
}

async fn probe_common_multicall(
    http: &crate::HttpContext,
    control: &crate::dapp_request::DappRequestControl,
    chain_id: u64,
    endpoints: &[SensitiveUrl],
) -> Result<Option<Address>, RpcBrokerError> {
    control.ensure_current()?;
    // Exercise the same tryAggregate entry point used by the RPC broker, with
    // a self-call that needs no account data. Every saved endpoint must support it.
    let probe = async {
        for endpoint in endpoints {
            control.ensure_current()?;
            let provider = ProviderBuilder::new()
                .connect_reqwest(http.rpc_client.clone(), endpoint.expose_url().clone());
            let result = provider
                .multicall()
                .get_chain_id()
                .try_aggregate(false)
                .await;
            control.ensure_current()?;
            if !matches!(result, Ok((Ok(id),)) if id == U256::from(chain_id)) {
                return Ok(None);
            }
        }
        Ok(Some(MULTICALL3_ADDRESS))
    };
    // Bound the whole optional probe, rather than adding a timeout per endpoint.
    let result = tokio::select! {
        biased;
        () = control.cancelled() => return Err(RpcBrokerError::OriginRejected),
        result = tokio::time::timeout(std::time::Duration::from_secs(5), probe) => result,
    };
    control.ensure_current()?;
    result.unwrap_or(Ok(None))
}

pub async fn watch_asset_metadata(
    reads: &DappRpcReadClient,
    control: &crate::dapp_request::DappRequestControl,
    endpoint: SensitiveUrl,
    chain_id: u64,
    address: Address,
) -> Result<CustomTokenSettings, RpcBrokerError> {
    control.ensure_current()?;
    let symbol: Bytes = reads
        .eth_call(
            endpoint.clone(),
            chain_id,
            address,
            RpcBrokerViewCalls::symbolCall {}.abi_encode().into(),
        )
        .await?;
    control.ensure_current()?;
    let symbol = RpcBrokerViewCalls::symbolCall::abi_decode_returns_validate(&symbol)
        .map_err(|_| RpcBrokerError::InvalidResponse)?;
    let decimals: Bytes = reads
        .eth_call(
            endpoint,
            chain_id,
            address,
            RpcBrokerViewCalls::decimalsCall {}.abi_encode().into(),
        )
        .await?;
    control.ensure_current()?;
    let decimals = RpcBrokerViewCalls::decimalsCall::abi_decode_returns_validate(&decimals)
        .map_err(|_| RpcBrokerError::InvalidResponse)?;
    Ok(CustomTokenSettings {
        chain_id,
        token_address: address.to_string(),
        symbol,
        decimals,
        icon_path: None,
        price_anchor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dapp_request::DappRequestControl;
    use alloy::providers::bindings::IMulticall3;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn proposed_endpoint_timeout_finishes_before_approval_expiry() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint =
            url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let control = DappRequestControl::new(
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
            || Ok(()),
        );
        let active = control.clone();
        let http = crate::HttpContext::direct_for_tests();
        let verification = tokio::spawn(async move {
            verify_chain_endpoints(&http, &active, 123, vec![endpoint.into()]).await
        });
        let connection = tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(11)).await;
        assert!(verification.await.unwrap().is_err());
        assert!(control.ensure_current().is_ok());
        drop(connection);
    }

    #[tokio::test]
    async fn proposed_endpoints_require_every_identity_and_stop_on_retirement() {
        for mode in ["match", "mismatch", "retire", "expired", "offline"] {
            let control = DappRequestControl::new(
                tokio::time::Instant::now() + std::time::Duration::from_secs(30),
                || Ok(()),
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let active = control.clone();
            let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
                Arc::new(move |request| {
                    if request["method"] == "eth_call" {
                        assert_eq!(mode, "match");
                        assert_eq!(seen.load(Ordering::SeqCst), 2);
                        return serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":"0x"});
                    }
                    assert_eq!(request["method"], "eth_chainId");
                    let index = seen.fetch_add(1, Ordering::SeqCst);
                    if mode == "retire" { active.invalidate(&RpcBrokerError::OriginRejected); }
                    serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result": if mode == "mismatch" && index == 1 {"0x1"} else {"0x20000000000001"}})
                }), Arc::default(), Arc::default(),
            ).await;
            let mut http = crate::HttpContext::direct_for_tests();
            if mode == "expired" {
                control.invalidate(&RpcBrokerError::Timeout);
            }
            if mode == "offline" {
                // A configured, unavailable proxy must prevent direct endpoint contact.
                let unavailable = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = unavailable.local_addr().unwrap();
                drop(unavailable);
                http.rpc_client = reqwest::Client::builder()
                    .proxy(reqwest::Proxy::all(format!("http://{address}")).unwrap())
                    .build()
                    .unwrap();
            }
            let result = verify_chain_endpoints(
                &http,
                &control,
                9_007_199_254_740_993,
                vec![endpoint.clone().into(), endpoint.into()],
            )
            .await;
            assert_eq!(result.is_ok(), mode == "match");
            assert_eq!(
                calls.load(Ordering::SeqCst),
                match mode {
                    "match" | "mismatch" => 2,
                    "retire" => 1,
                    _ => 0,
                }
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn chain_add_discovers_multicall_only_when_every_endpoint_can_batch() {
        const CHAIN_ID: u64 = 9_007_199_254_740_993;
        for mode in [
            "available",
            "absent",
            "malformed",
            "revert",
            "inner-failure",
            "wrong-chain",
            "retired",
        ] {
            let control = DappRequestControl::new(
                tokio::time::Instant::now() + std::time::Duration::from_secs(30),
                || Ok(()),
            );
            let probes = Arc::new(AtomicUsize::new(0));
            let seen = probes.clone();
            let active = control.clone();
            let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
                Arc::new(move |request| {
                    if request["method"] == "eth_chainId" {
                        assert_eq!(seen.load(Ordering::SeqCst), 0);
                        return serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":"0x20000000000001"});
                    }
                    assert_eq!(request["method"], "eth_call");
                    assert_eq!(request["params"][0]["to"], serde_json::json!(MULTICALL3_ADDRESS));
                    assert!(request["params"][0].get("from").is_none());
                    let input: Bytes = serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
                    let call = IMulticall3::tryAggregateCall::abi_decode(&input).unwrap();
                    assert!(!call.requireSuccess);
                    assert_eq!(call.calls.len(), 1);
                    assert_eq!(call.calls[0].target, MULTICALL3_ADDRESS);
                    assert_eq!(call.calls[0].callData.as_ref(), IMulticall3::getChainIdCall {}.abi_encode());
                    // The first endpoint works; only the second exercises the failure.
                    let second = seen.fetch_add(1, Ordering::SeqCst) == 1;
                    if second && mode == "retired" {
                        active.invalidate(&RpcBrokerError::OriginRejected);
                    }
                    if second && mode == "revert" {
                        return serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":3,"message":"execution reverted"}});
                    }
                    let result = if second && mode == "absent" {
                        Bytes::new()
                    } else if second && mode == "malformed" {
                        Bytes::from_static(&[1, 2, 3])
                    } else {
                        let chain_id = if second && mode == "wrong-chain" { 1 } else { CHAIN_ID };
                        IMulticall3::tryAggregateCall::abi_encode_returns(&vec![IMulticall3::Result {
                            success: !(second && mode == "inner-failure"),
                            returnData: IMulticall3::getChainIdCall::abi_encode_returns(&U256::from(chain_id)).into(),
                        }]).into()
                    };
                    serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
                }),
                Arc::default(),
                Arc::default(),
            ).await;
            let result = verify_chain_endpoints(
                &crate::HttpContext::direct_for_tests(),
                &control,
                CHAIN_ID,
                vec![endpoint.clone().into(), endpoint.into()],
            )
            .await;
            assert_eq!(
                result,
                match mode {
                    "available" => Ok(Some(MULTICALL3_ADDRESS)),
                    "retired" => Err(RpcBrokerError::OriginRejected),
                    _ => Ok(None),
                },
                "{mode}"
            );
            assert_eq!(probes.load(Ordering::SeqCst), 2);
            server.abort();
        }
    }

    #[tokio::test]
    async fn multicall_probe_timeout_is_optional_but_expiry_and_retirement_are_not() {
        for mode in ["timeout", "expired", "retired"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint =
                url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let control = DappRequestControl::new(
                tokio::time::Instant::now()
                    + std::time::Duration::from_secs(if mode == "expired" { 1 } else { 30 }),
                || Ok(()),
            );
            let active = control.clone();
            let probe = tokio::spawn(async move {
                probe_common_multicall(
                    &crate::HttpContext::direct_for_tests(),
                    &active,
                    8453,
                    &[endpoint.into()],
                )
                .await
            });
            let connection =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            tokio::time::pause();
            if mode == "retired" {
                control.invalidate(&RpcBrokerError::OriginRejected);
            }
            tokio::time::advance(std::time::Duration::from_secs(6)).await;
            let result = probe.await.unwrap();
            if mode == "timeout" {
                assert_eq!(result, Ok(None));
                assert!(control.ensure_current().is_ok());
            } else {
                assert!(result.is_err());
            }
            drop(connection);
            tokio::time::resume();
        }
    }

    #[tokio::test]
    async fn metadata_requires_both_abi_results_and_stops_after_invalidation() {
        for mode in ["complete", "cancel", "invalid-symbol"] {
            let control = DappRequestControl::new(
                tokio::time::Instant::now() + std::time::Duration::from_secs(30),
                || Ok(()),
            );
            let count = Arc::new(AtomicUsize::new(0));
            let reads = DappRpcReadClient::new({
                let count = count.clone();
                let control = control.clone();
                move |_, _| {
                    let index = count.fetch_add(1, Ordering::SeqCst);
                    if index == 0 && mode == "cancel" {
                        control.invalidate(&RpcBrokerError::OriginRejected);
                    }
                    let bytes = if index == 0 {
                        if mode == "invalid-symbol" {
                            vec![1]
                        } else {
                            RpcBrokerViewCalls::symbolCall::abi_encode_returns(
                                &"ONCHAIN".to_owned(),
                            )
                        }
                    } else {
                        RpcBrokerViewCalls::decimalsCall::abi_encode_returns(&6)
                    };
                    Box::pin(async move { Ok(serde_json::to_value(Bytes::from(bytes)).unwrap()) })
                }
            });
            let result = watch_asset_metadata(
                &reads,
                &control,
                url::Url::parse("http://127.0.0.1:1").unwrap().into(),
                1,
                Address::ZERO,
            )
            .await;
            if mode == "complete" {
                let token = result.unwrap();
                assert_eq!((token.symbol.as_str(), token.decimals), ("ONCHAIN", 6));
                assert_eq!(count.load(Ordering::SeqCst), 2);
            } else {
                assert!(result.is_err());
                assert_eq!(count.load(Ordering::SeqCst), 1);
            }
        }
    }
}
