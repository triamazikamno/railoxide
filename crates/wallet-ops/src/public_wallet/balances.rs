use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::{Instant, SystemTime};

use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes, U64, U256};
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use alloy::serde::WithOtherFields;
use alloy::sol_types::SolCall;
use eyre::{Result, WrapErr, eyre};
use railgun_ui::known_tokens_for_chain;
use serde_json::Value;

use super::contracts::PublicErc20;
use super::runtime::public_chain_runtime_config;
use super::types::{
    PlannedPublicBalanceCall, PublicAccountBalance, PublicAssetId, PublicBalanceAmount,
    PublicBalanceAsset, PublicBalanceEntry, PublicBalanceSnapshot,
};
use crate::rpc_broker::total_failure;
use crate::settings::{EffectiveChainConfig, EffectiveTokenRegistry};
use crate::vault::PublicAccountMetadata;
use crate::{
    HttpContext, RpcBrokerError, RpcRead, RpcResult, RpcRoute, RpcSubmission, WalletRpcOrigin,
};

const PUBLIC_BALANCE_REFRESH_INTERVAL_SECS: u64 = 60;

#[must_use]
pub const fn public_balance_refresh_interval_secs() -> u64 {
    PUBLIC_BALANCE_REFRESH_INTERVAL_SECS
}

#[must_use]
pub fn public_balance_assets_for_chain(chain: &EffectiveChainConfig) -> Vec<PublicBalanceAsset> {
    public_balance_assets_for_chain_with_registry(chain, None)
}

#[must_use]
pub(super) fn public_balance_assets_for_chain_with_registry(
    chain: &EffectiveChainConfig,
    token_registry: Option<&EffectiveTokenRegistry>,
) -> Vec<PublicBalanceAsset> {
    let chain_id = chain.chain_id;
    let mut assets = vec![native_asset_for_chain(chain)];
    if let Some(token_registry) = token_registry {
        assets.extend(
            token_registry
                .tokens
                .values()
                .filter(|token| token.chain_id == chain_id)
                .filter_map(|token| {
                    Address::from_str(&token.token_address)
                        .ok()
                        .map(|address| PublicBalanceAsset {
                            id: PublicAssetId::Erc20(address),
                            symbol: token.symbol.clone(),
                            decimals: token.decimals,
                        })
                }),
        );
    } else {
        assets.extend(
            known_tokens_for_chain(chain_id).map(|token| PublicBalanceAsset {
                id: PublicAssetId::Erc20(token.token),
                symbol: token.symbol.to_string(),
                decimals: token.decimals,
            }),
        );
    }
    assets
}

pub(super) fn plan_public_balance_calls(
    chain: &EffectiveChainConfig,
    accounts: &[PublicAccountMetadata],
    token_registry: Option<&EffectiveTokenRegistry>,
    block: BlockId,
) -> Result<Vec<PlannedPublicBalanceCall>> {
    let chain_id = chain.chain_id;
    let assets = public_balance_assets_for_chain_with_registry(chain, token_registry);
    let mut calls = Vec::with_capacity(accounts.len().saturating_mul(assets.len()));
    for account in accounts {
        for asset in &assets {
            let read = match asset.id {
                PublicAssetId::Native => Ok(RpcRead::get_balance_at(account.address, block)),
                PublicAssetId::Erc20(token) => RpcRead::from_rpc(
                    WithOtherFields::new(TransactionRequest {
                        to: Some(token.into()),
                        input: TransactionInput::maybe_both(Some(Bytes::from(
                            PublicErc20::balanceOfCall {
                                account: account.address,
                            }
                            .abi_encode(),
                        ))),
                        ..TransactionRequest::default()
                    }),
                    block,
                    None,
                    chain_id,
                ),
            }?;
            calls.push(PlannedPublicBalanceCall {
                public_account_uuid: account.public_account_uuid.clone(),
                asset: asset.clone(),
                read,
            });
        }
    }
    Ok(calls)
}

pub async fn refresh_public_balances(
    chain_id: u64,
    account: &PublicAccountMetadata,
    effective_chain: &EffectiveChainConfig,
    token_registry: Option<&EffectiveTokenRegistry>,
    http: &HttpContext,
) -> Result<PublicBalanceSnapshot> {
    refresh_public_balances_at_least(
        chain_id,
        account,
        effective_chain,
        token_registry,
        http,
        None,
    )
    .await
}

/// Reads one account over its own isolated route, pinned to the block number read first on
/// that route. A number below `minimum` is an error before any balance read is sent.
pub async fn refresh_public_balances_at_least(
    chain_id: u64,
    account: &PublicAccountMetadata,
    effective_chain: &EffectiveChainConfig,
    token_registry: Option<&EffectiveTokenRegistry>,
    http: &HttpContext,
    minimum: Option<u64>,
) -> Result<PublicBalanceSnapshot> {
    let accounts = if account.is_available_on_chain(chain_id) {
        std::slice::from_ref(account)
    } else {
        &[]
    };
    let chain = public_chain_runtime_config(chain_id, effective_chain)?;
    if accounts.is_empty()
        || public_balance_assets_for_chain_with_registry(effective_chain, token_registry).is_empty()
    {
        return Ok(empty_public_balance_snapshot(chain_id, accounts));
    }
    let sensitive = chain
        .rpc_route
        .for_class(crate::RpcTrafficClass::Sensitive)?;
    // Never fall back to the shared route: that would mix this account with other traffic.
    let client = http
        .operation_http_client()
        .await
        .wrap_err("public balance isolated route unavailable")?;
    let route = RpcRoute::from(sensitive.isolated(client));
    let head = RpcRead::from_method_params("eth_blockNumber", Value::Array(Vec::new()), chain_id)?;
    let observed_block = submit_public_balance_reads(http, route.clone(), vec![head])
        .await?
        .pop()
        .and_then(std::result::Result::ok)
        .and_then(|result| serde_json::from_value::<U64>(result.into_value()).ok())
        .map(|number| number.to::<u64>())
        .ok_or_else(|| eyre!("public balance block number read failed"))?;
    if minimum.is_some_and(|minimum| observed_block < minimum) {
        return Err(eyre!(
            "public balance observed block does not satisfy minimum block"
        ));
    }
    // Pinning every read to the route's number keeps it valid for each request the broker
    // splits, chunks or fails over the aggregate into.
    let planned_calls = plan_public_balance_calls(
        effective_chain,
        accounts,
        token_registry,
        BlockId::number(observed_block),
    )?;
    let reads: Vec<RpcRead> = planned_calls.iter().map(|call| call.read.clone()).collect();
    let observed_at = Instant::now();
    let results = submit_public_balance_reads(http, route, reads).await?;
    if let Some(error) = total_failure(&results) {
        tracing::warn!(
            chain_id,
            account_count = accounts.len(),
            member_count = results.len(),
            failure_class = ?error.failure_class(),
            "public balance RPC request failed for all members"
        );
        return Err(eyre!(
            "public balance RPC request failed for all members: {error}"
        ));
    }
    tracing::debug!(
        chain_id,
        account_count = accounts.len(),
        member_count = results.len(),
        "public balance RPC refresh completed"
    );
    Ok(public_balance_snapshot_from_results(
        chain_id,
        accounts,
        &planned_calls,
        results
            .into_iter()
            .zip(&planned_calls)
            .map(|(result, call)| {
                result
                    .as_ref()
                    .ok()
                    .and_then(|value| decode_public_balance(&call.asset.id, value))
            })
            .collect(),
        observed_at,
        Some(observed_block),
    ))
}

async fn submit_public_balance_reads(
    http: &HttpContext,
    route: RpcRoute,
    reads: Vec<RpcRead>,
) -> Result<Vec<Result<RpcResult, RpcBrokerError>>> {
    http.rpc_broker()
        .submit(RpcSubmission::new(
            route,
            reads,
            WalletRpcOrigin::PublicWallet.into(),
        ))
        .await
        .map_err(|error| eyre!("RPC broker balance submission failed: {error}"))
}

fn decode_public_balance(asset: &PublicAssetId, result: &RpcResult) -> Option<U256> {
    match asset {
        PublicAssetId::Native => serde_json::from_value(result.expose_value().clone()).ok(),
        PublicAssetId::Erc20(_) => {
            let bytes: Bytes = serde_json::from_value(result.expose_value().clone()).ok()?;
            PublicErc20::balanceOfCall::abi_decode_returns_validate(&bytes).ok()
        }
    }
}

pub(super) fn public_balance_snapshot_from_results(
    chain_id: u64,
    accounts: &[PublicAccountMetadata],
    planned_calls: &[PlannedPublicBalanceCall],
    results: Vec<Option<U256>>,
    observed_at: Instant,
    observed_block: Option<u64>,
) -> PublicBalanceSnapshot {
    let expected_counts =
        planned_calls
            .iter()
            .fold(BTreeMap::<&str, usize>::new(), |mut counts, call| {
                *counts.entry(&call.public_account_uuid).or_default() += 1;
                counts
            });
    let mut by_account: BTreeMap<String, Vec<PublicBalanceEntry>> = BTreeMap::new();
    for (call, result) in planned_calls.iter().zip(results) {
        by_account
            .entry(call.public_account_uuid.clone())
            .or_default()
            .push(PublicBalanceEntry {
                asset: call.asset.clone(),
                amount: result.map_or(
                    PublicBalanceAmount::Unavailable,
                    PublicBalanceAmount::Available,
                ),
            });
    }

    PublicBalanceSnapshot {
        chain_id,
        refreshed_at: SystemTime::now(),
        accounts: accounts
            .iter()
            .cloned()
            .map(|account| {
                let balances = by_account
                    .remove(&account.public_account_uuid)
                    .unwrap_or_default();
                let complete = observed_block.is_some()
                    && !balances.is_empty()
                    && expected_counts.get(account.public_account_uuid.as_str())
                        == Some(&balances.len())
                    && balances
                        .iter()
                        .all(|balance| balance.amount.amount().is_some());
                PublicAccountBalance {
                    balances,
                    account,
                    observed_at: complete.then_some(observed_at),
                    observed_block: observed_block.filter(|_| complete),
                }
            })
            .collect(),
    }
}

fn empty_public_balance_snapshot(
    chain_id: u64,
    accounts: &[PublicAccountMetadata],
) -> PublicBalanceSnapshot {
    PublicBalanceSnapshot {
        chain_id,
        refreshed_at: SystemTime::now(),
        accounts: accounts
            .iter()
            .cloned()
            .map(|account| PublicAccountBalance {
                account,
                balances: Vec::new(),
                observed_at: None,
                observed_block: None,
            })
            .collect(),
    }
}

pub(crate) fn native_asset_for_chain(chain: &EffectiveChainConfig) -> PublicBalanceAsset {
    PublicBalanceAsset {
        id: PublicAssetId::Native,
        symbol: chain.native_currency.symbol.clone(),
        decimals: chain.native_currency.decimals,
    }
}
