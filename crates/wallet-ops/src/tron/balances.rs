//! Tron account balances. Tron's JSON-RPC runs `eth_call` only at `latest`,
//! so each account is read with one Multicall3 `aggregate3` holding `getBlockNumber`,
//! `getEthBalance` and each token's `balanceOf`. One call executes against one state, and its
//! block number stands in for the block that EVM balance reads are pinned to.

use std::time::Instant;

use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::bindings::IMulticall3;
use alloy::sol_types::SolCall;
use eyre::{Result, WrapErr, eyre};

use super::TRON_CHAIN_ID;
use crate::public_wallet::{PublicErc20, registry_balance_assets};
use crate::settings::EffectiveTokenRegistry;
use crate::vault::TronAccountMetadata;
use crate::{
    ClassRoute, HttpContext, PublicAssetId, PublicBalanceAmount, PublicBalanceAsset,
    PublicBalanceEntry, RpcChainRoute, RpcRoute, RpcTrafficClass, WalletRpcOrigin,
};

/// One Tron account's balances, all from one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TronAccountBalance {
    pub account: TronAccountMetadata,
    /// Ordered as [`tron_balance_assets`]. A failed entry is unavailable, never zero.
    pub balances: Vec<PublicBalanceEntry>,
    /// Monotonic start of the read that was accepted; never persisted.
    pub observed_at: Instant,
    /// The block the call executed at.
    pub observed_block: u64,
}

/// TRX, in sun, then each Tron token of `token_registry`.
#[must_use]
pub fn tron_balance_assets(token_registry: &EffectiveTokenRegistry) -> Vec<PublicBalanceAsset> {
    let trx = railgun_ui::tron_native_currency();
    std::iter::once(PublicBalanceAsset {
        id: PublicAssetId::Native,
        symbol: trx.symbol,
        decimals: trx.decimals,
    })
    .chain(registry_balance_assets(token_registry, TRON_CHAIN_ID))
    .collect()
}

/// Reads `account`'s balances on `route`'s Sensitive endpoints, over an isolated route opened
/// for this refresh alone, as one unsplit call at `latest`. A result from a block below
/// `minimum` isn't current: the read moves on to the next eligible endpoint, and fails when no
/// endpoint reaches `minimum`.
pub async fn refresh_tron_balances_at_least(
    account: &TronAccountMetadata,
    route: &RpcChainRoute,
    token_registry: &EffectiveTokenRegistry,
    http: &HttpContext,
    minimum: Option<u64>,
) -> Result<TronAccountBalance> {
    let multicall = route
        .multicall()
        .ok_or_else(|| eyre!("Tron route has no Multicall3"))?;
    let assets = tron_balance_assets(token_registry);
    let calldata = balance_calldata(multicall, account.address, &assets);
    let sensitive = route.for_class(RpcTrafficClass::Sensitive)?;
    // Never fall back to the shared route: that would mix this account with other traffic.
    let client = http
        .operation_http_client()
        .await
        .wrap_err("Tron balance isolated route unavailable")?;
    let endpoints = http
        .rpc_broker()
        .endpoint_candidates(sensitive.route(), None)
        .await?;
    let mut last_error = eyre!("no Tron endpoint answered the balance read");
    for endpoint in endpoints {
        // One endpoint without Multicall3 batching, so the broker neither splits nor wraps the
        // call, and a result below `minimum` can be retried elsewhere. Each attempt gets its own
        // isolation over the same client, so none is answered from another's cached result.
        let attempt = sensitive
            .route()
            .for_endpoint(endpoint)
            .without_multicall()
            .for_class(RpcTrafficClass::Sensitive)?
            .isolated(client.clone());
        let observed_at = Instant::now();
        match read_balances(http, attempt, multicall, calldata.clone(), assets.len()).await {
            Ok((block, _)) if minimum.is_some_and(|minimum| block < minimum) => {
                last_error = eyre!("Tron balance observed block does not satisfy minimum block");
            }
            Ok((observed_block, amounts)) => {
                return Ok(TronAccountBalance {
                    account: account.clone(),
                    balances: assets
                        .into_iter()
                        .zip(amounts)
                        .map(|(asset, amount)| PublicBalanceEntry {
                            asset,
                            amount: amount.map_or(
                                PublicBalanceAmount::Unavailable,
                                PublicBalanceAmount::Available,
                            ),
                        })
                        .collect(),
                    observed_at,
                    observed_block,
                });
            }
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

/// `aggregate3` of `getBlockNumber`, which must succeed, then one call per asset.
fn balance_calldata(multicall: Address, account: Address, assets: &[PublicBalanceAsset]) -> Bytes {
    let block = IMulticall3::Call3 {
        target: multicall,
        allowFailure: false,
        callData: IMulticall3::getBlockNumberCall {}.abi_encode().into(),
    };
    let balances = assets.iter().map(|asset| match asset.id {
        PublicAssetId::Native => IMulticall3::Call3 {
            target: multicall,
            allowFailure: true,
            callData: IMulticall3::getEthBalanceCall { addr: account }
                .abi_encode()
                .into(),
        },
        PublicAssetId::Erc20(token) => IMulticall3::Call3 {
            target: token,
            allowFailure: true,
            callData: PublicErc20::balanceOfCall { account }.abi_encode().into(),
        },
    });
    IMulticall3::aggregate3Call {
        calls: std::iter::once(block).chain(balances).collect(),
    }
    .abi_encode()
    .into()
}

/// One call on `route`: its block number, then each asset's amount, `None` where it failed.
async fn read_balances(
    http: &HttpContext,
    route: ClassRoute,
    multicall: Address,
    calldata: Bytes,
    asset_count: usize,
) -> Result<(u64, Vec<Option<U256>>)> {
    let results = http
        .rpc_broker()
        .submit_calls_decoded_as::<IMulticall3::aggregate3Call>(
            RpcRoute::from(route),
            vec![(multicall, calldata)],
            WalletRpcOrigin::PublicWallet.into(),
        )
        .await
        .map_err(|error| eyre!("RPC broker Tron balance submission failed: {error}"))?
        .pop()
        .ok_or_else(|| eyre!("Tron balance read returned no result"))?
        .map_err(|error| eyre!("Tron balance read failed: {error}"))?;
    let Some((head, balances)) = results.split_first() else {
        return Err(eyre!("Tron balance result is empty"));
    };
    if balances.len() != asset_count {
        return Err(eyre!("Tron balance result has the wrong number of entries"));
    }
    let block = IMulticall3::getBlockNumberCall::abi_decode_returns_validate(&head.returnData)
        .ok()
        .filter(|_| head.success)
        .and_then(|block| u64::try_from(block).ok())
        .ok_or_else(|| eyre!("Tron balance block number does not decode"))?;
    // `getEthBalance` and `balanceOf` both return one `uint256`.
    let amounts = balances
        .iter()
        .map(|result| {
            if result.success {
                PublicErc20::balanceOfCall::abi_decode_returns_validate(&result.returnData).ok()
            } else {
                None
            }
        })
        .collect();
    Ok((block, amounts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc_broker::tests::{
        RecordedRequests, RpcResponder, spawn_recording_rpc_mock, tagged_reqwest_client,
    };
    use crate::settings::{EffectiveTokenInfo, TronEndpointSetting, TronSettings};
    use crate::tron::tron_rpc_route;
    use crate::vault::{PublicAccountScope, PublicAccountStatus, TronAccountSource};
    use crate::{OperationHttpClient, OperationNetworkIsolation};
    use serde_json::{Value, json};
    use std::sync::Arc;

    const ACCOUNT: Address = Address::repeat_byte(7);
    const USDT: Address = Address::repeat_byte(0x11);
    const USDD: Address = Address::repeat_byte(0x22);

    fn account() -> TronAccountMetadata {
        TronAccountMetadata {
            tron_account_uuid: "tron-1".to_owned(),
            address: ACCOUNT,
            label: None,
            source: TronAccountSource::Imported,
            scope: PublicAccountScope::PrivateWallet {
                wallet_uuid: "wallet-1".to_owned(),
            },
            status: PublicAccountStatus::Active,
            display_order: 0,
        }
    }

    /// USDT and USDD on Tron, and a token of another chain that the read leaves out.
    fn registry() -> EffectiveTokenRegistry {
        let token = |chain_id, address: Address, symbol: &str| {
            (
                (chain_id, address.to_string()),
                EffectiveTokenInfo {
                    chain_id,
                    token_address: address.to_string(),
                    symbol: symbol.to_owned(),
                    decimals: 6,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            )
        };
        EffectiveTokenRegistry {
            tokens: [
                token(TRON_CHAIN_ID, USDT, "USDT"),
                token(TRON_CHAIN_ID, USDD, "USDD"),
                token(1, Address::repeat_byte(0x33), "OTHER"),
            ]
            .into(),
        }
    }

    fn word(value: u64) -> Bytes {
        Bytes::from(U256::from(value).to_be_bytes::<32>().to_vec())
    }

    /// The `aggregate3` calls of a recorded `eth_call`.
    fn calls(request: &Value) -> Vec<IMulticall3::Call3> {
        let call = &request["params"][0];
        let input = call
            .get("input")
            .or_else(|| call.get("data"))
            .and_then(Value::as_str)
            .unwrap()
            .parse::<Bytes>()
            .unwrap();
        IMulticall3::aggregate3Call::abi_decode(&input)
            .unwrap()
            .calls
    }

    /// A mainnet Tron node at `block`, holding 5 sun, 9 USDT and no readable USDD.
    fn node(block: u64) -> RpcResponder {
        Arc::new(move |request| {
            let result = if request["method"] == "eth_chainId" {
                json!("0x2b6653dc")
            } else {
                let returns = calls(&request)
                    .iter()
                    .map(|call| {
                        let (success, data) = if call.target == USDD {
                            (false, Bytes::new())
                        } else if call.target == USDT {
                            (true, word(9))
                        } else if call
                            .callData
                            .starts_with(&IMulticall3::getBlockNumberCall::SELECTOR)
                        {
                            (true, word(block))
                        } else {
                            (true, word(5))
                        };
                        IMulticall3::Result {
                            success,
                            returnData: data,
                        }
                    })
                    .collect::<Vec<_>>();
                json!(Bytes::from(
                    IMulticall3::aggregate3Call::abi_encode_returns(&returns)
                ))
            };
            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        })
    }

    fn route(nodes: &[&url::Url]) -> RpcChainRoute {
        tron_rpc_route(&TronSettings {
            endpoints: Some(
                nodes
                    .iter()
                    .map(|url| TronEndpointSetting {
                        json_rpc_url: url.to_string(),
                        ..TronEndpointSetting::default()
                    })
                    .collect(),
            ),
            ..TronSettings::default()
        })
        .unwrap()
    }

    /// Wallet traffic whose operation clients tag their requests `account`.
    fn http() -> HttpContext {
        HttpContext::direct_for_tests().with_operation_clients_for_tests(|| {
            Ok(OperationHttpClient::for_tests(
                tagged_reqwest_client("account"),
                OperationNetworkIsolation::Dedicated,
            ))
        })
    }

    /// Requests other than identity checks.
    fn reads(recorded: &RecordedRequests) -> Vec<(Option<String>, Value)> {
        recorded
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, body)| body["method"] != "eth_chainId")
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn one_account_is_read_in_one_call_over_its_own_route() {
        let (url, recorded, _server) = spawn_recording_rpc_mock(node(100)).await;
        let route = route(&[&url]);

        let balance =
            refresh_tron_balances_at_least(&account(), &route, &registry(), &http(), None)
                .await
                .unwrap();

        let reads = reads(&recorded);
        assert_eq!(reads.len(), 1, "one unsplit call");
        let (tag, request) = &reads[0];
        assert_eq!(tag.as_deref(), Some("account"), "over the isolated route");
        assert_eq!(request["method"], "eth_call");
        assert_eq!(request["params"][1], "latest");
        let multicall = route.multicall().unwrap();
        assert_eq!(request["params"][0]["to"], json!(multicall));
        let expected = [
            (multicall, IMulticall3::getBlockNumberCall {}.abi_encode()),
            (
                multicall,
                IMulticall3::getEthBalanceCall { addr: ACCOUNT }.abi_encode(),
            ),
            (
                USDT,
                PublicErc20::balanceOfCall { account: ACCOUNT }.abi_encode(),
            ),
            (
                USDD,
                PublicErc20::balanceOfCall { account: ACCOUNT }.abi_encode(),
            ),
        ];
        let sent = calls(request)
            .into_iter()
            .map(|call| (call.target, call.callData.to_vec()))
            .collect::<Vec<_>>();
        assert_eq!(
            sent, expected,
            "only this account, not wrapped in tryAggregate"
        );

        assert_eq!(balance.observed_block, 100);
        let amounts = balance
            .balances
            .iter()
            .map(|entry| (entry.asset.id, entry.amount.amount()))
            .collect::<Vec<_>>();
        assert_eq!(
            amounts,
            [
                (PublicAssetId::Native, Some(U256::from(5))),
                (PublicAssetId::Erc20(USDT), Some(U256::from(9))),
                (PublicAssetId::Erc20(USDD), None),
            ]
        );
    }

    #[tokio::test]
    async fn a_result_below_the_minimum_is_retried_on_another_endpoint() {
        let (behind, behind_log, _behind) = spawn_recording_rpc_mock(node(99)).await;
        let (current, current_log, _current) = spawn_recording_rpc_mock(node(100)).await;
        // Test contexts try candidates in configured order, so the endpoint behind goes first.
        let route = route(&[&behind, &current]);

        let balance =
            refresh_tron_balances_at_least(&account(), &route, &registry(), &http(), Some(100))
                .await
                .unwrap();
        assert_eq!(balance.observed_block, 100);
        assert_eq!(reads(&behind_log).len(), 1);
        assert_eq!(reads(&current_log).len(), 1);

        // No endpoint reaches the minimum.
        assert!(
            refresh_tron_balances_at_least(&account(), &route, &registry(), &http(), Some(101))
                .await
                .is_err()
        );
    }
}
