use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::SystemTime;

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;
use eyre::{Result, eyre};
use railgun_ui::known_tokens_for_chain;

use super::contracts::PublicErc20;
use super::runtime::public_chain_runtime_config;
use super::types::{
    PlannedPublicBalanceCall, PublicAccountBalance, PublicAssetId, PublicBalanceAmount,
    PublicBalanceAsset, PublicBalanceEntry, PublicBalanceSnapshot,
};
use crate::rpc_broker::total_failure;
use crate::settings::{EffectiveChainConfig, EffectiveTokenRegistry};
use crate::vault::PublicAccountMetadata;
use crate::{HttpContext, RpcRead, RpcRoute, RpcSubmission, WalletRpcOrigin};

const PUBLIC_BALANCE_REFRESH_INTERVAL_SECS: u64 = 60;

#[must_use]
pub const fn public_balance_refresh_interval_secs() -> u64 {
    PUBLIC_BALANCE_REFRESH_INTERVAL_SECS
}

#[must_use]
pub fn public_balance_assets_for_chain(chain_id: u64) -> Vec<PublicBalanceAsset> {
    public_balance_assets_for_chain_with_registry(chain_id, None)
}

#[must_use]
pub(super) fn public_balance_assets_for_chain_with_registry(
    chain_id: u64,
    token_registry: Option<&EffectiveTokenRegistry>,
) -> Vec<PublicBalanceAsset> {
    let mut assets = Vec::new();
    if let Some(native) = native_asset_for_chain(chain_id) {
        assets.push(native);
    }
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
    chain_id: u64,
    accounts: &[PublicAccountMetadata],
    token_registry: Option<&EffectiveTokenRegistry>,
) -> Vec<PlannedPublicBalanceCall> {
    let assets = public_balance_assets_for_chain_with_registry(chain_id, token_registry);
    let mut calls = Vec::with_capacity(accounts.len().saturating_mul(assets.len()));
    for account in accounts {
        for asset in &assets {
            let read = match asset.id {
                PublicAssetId::Native => RpcRead::get_balance(account.address),
                PublicAssetId::Erc20(token) => RpcRead::eth_call(
                    token,
                    PublicErc20::balanceOfCall {
                        account: account.address,
                    }
                    .abi_encode()
                    .into(),
                ),
            };
            calls.push(PlannedPublicBalanceCall {
                public_account_uuid: account.public_account_uuid.clone(),
                asset: asset.clone(),
                read,
            });
        }
    }
    calls
}

pub async fn refresh_public_balances(
    chain_id: u64,
    accounts: &[PublicAccountMetadata],
    effective_chain: Option<&EffectiveChainConfig>,
    token_registry: Option<&EffectiveTokenRegistry>,
    http: &HttpContext,
) -> Result<PublicBalanceSnapshot> {
    let chain = public_chain_runtime_config(chain_id, effective_chain)?;
    let planned_calls = plan_public_balance_calls(chain_id, accounts, token_registry);
    if planned_calls.is_empty() {
        return Ok(empty_public_balance_snapshot(chain_id, accounts));
    }

    let route = RpcRoute::from(chain.rpc_route);
    let reads = planned_calls.iter().map(|call| call.read.clone()).collect();
    let results = http
        .rpc_broker()
        .submit(RpcSubmission::new(
            route,
            reads,
            WalletRpcOrigin::PublicWallet.into(),
        ))
        .await
        .map_err(|error| eyre!("RPC broker balance submission failed: {error}"))?;
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
            .map(|result| result.as_ref().ok().and_then(decode_public_balance))
            .collect(),
    ))
}

fn decode_public_balance(bytes: &Bytes) -> Option<U256> {
    PublicErc20::balanceOfCall::abi_decode_returns_validate(bytes).ok()
}

pub(super) fn public_balance_snapshot_from_results(
    chain_id: u64,
    accounts: &[PublicAccountMetadata],
    planned_calls: &[PlannedPublicBalanceCall],
    results: Vec<Option<U256>>,
) -> PublicBalanceSnapshot {
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
            .map(|account| PublicAccountBalance {
                balances: by_account
                    .remove(&account.public_account_uuid)
                    .unwrap_or_default(),
                account,
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
            })
            .collect(),
    }
}

fn native_asset_for_chain(chain_id: u64) -> Option<PublicBalanceAsset> {
    let symbol = match chain_id {
        1 | 42161 => "ETH",
        56 => "BNB",
        137 => "MATIC",
        _ => return None,
    };
    Some(PublicBalanceAsset {
        id: PublicAssetId::Native,
        symbol: symbol.to_string(),
        decimals: 18,
    })
}
