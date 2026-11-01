//! Tron's HTTP API, for what JSON-RPC can't serve: account existence and
//! resources, fee parameters, a block's transaction outcomes, and broadcast.
//!
//! Each call takes its traffic class from what it reveals: everything here names an
//! account or belongs to a Send, so reads are Sensitive and broadcast is Submission. Native USD
//! and price anchors read through JSON-RPC as General traffic. Only endpoints with an HTTP API URL
//! serve these calls, over each endpoint's transport or the route's isolated client.

use super::address::encode_tron_address;
use super::fees::{AccountResources, ChainFeeParameters};
use super::transaction::SignedTransaction;
use crate::{HttpContext, RpcBrokerError, RpcChainRoute, RpcTrafficClass};
use alloy::primitives::{Address, B256, Bytes, Log};
use serde::Deserialize;
use serde_json::{Value, json};

/// Tron's HTTP API over a Tron route; see [`super::tron_rpc_route`]. Pass an isolated route for a
/// per-account Tor route.
#[derive(Clone)]
pub struct TronHttpApi {
    http: HttpContext,
    route: RpcChainRoute,
}

/// One transaction's outcome, from `gettransactioninfobyblocknum`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TronTransactionInfo {
    pub id: B256,
    /// Not marked `FAILED`, and a contract call's receipt is `SUCCESS`.
    pub succeeded: bool,
    pub logs: Vec<Log>,
    /// The TRX the transaction burned, in sun.
    pub fee_sun: u64,
}

/// A node's answer to `broadcasthex`. It never decides a send's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TronBroadcast {
    pub accepted: bool,
    /// Tron's response code, such as `DUP_TRANSACTION_ERROR`, when the node gave one.
    pub code: Option<String>,
}

impl TronHttpApi {
    #[must_use]
    pub const fn new(http: HttpContext, route: RpcChainRoute) -> Self {
        Self { http, route }
    }

    async fn call(
        &self,
        class: RpcTrafficClass,
        method: &str,
        body: &Value,
    ) -> Result<Value, RpcBrokerError> {
        self.route
            .for_class(class)?
            .tron_http_api_call(&self.http, method, body)
            .await
    }

    /// Whether `account` exists on chain. An address that has only received TRC-20 tokens
    /// doesn't.
    pub async fn account_exists(&self, account: Address) -> Result<bool, RpcBrokerError> {
        let value = self
            .call(
                RpcTrafficClass::Sensitive,
                "getaccount",
                &account_body(account),
            )
            .await?;
        decode_account_exists(&value)
    }

    pub async fn account_resources(
        &self,
        account: Address,
    ) -> Result<AccountResources, RpcBrokerError> {
        let value = self
            .call(
                RpcTrafficClass::Sensitive,
                "getaccountresource",
                &account_body(account),
            )
            .await?;
        decode_account_resources(value)
    }

    /// Read only within a Send, so Sensitive.
    pub async fn chain_fee_parameters(&self) -> Result<ChainFeeParameters, RpcBrokerError> {
        let value = self
            .call(RpcTrafficClass::Sensitive, "getchainparameters", &json!({}))
            .await?;
        decode_chain_fee_parameters(value)
    }

    /// Every transaction outcome in block `number`. Read after a broadcast, so Sensitive.
    pub async fn transaction_infos(
        &self,
        number: u64,
    ) -> Result<Vec<TronTransactionInfo>, RpcBrokerError> {
        let value = self
            .call(
                RpcTrafficClass::Sensitive,
                "gettransactioninfobyblocknum",
                &json!({ "num": number }),
            )
            .await?;
        decode_transaction_infos(value)
    }

    pub async fn broadcast(
        &self,
        transaction: &SignedTransaction,
    ) -> Result<TronBroadcast, RpcBrokerError> {
        let value = self
            .call(
                RpcTrafficClass::Submission,
                "broadcasthex",
                &json!({ "transaction": transaction.to_hex() }),
            )
            .await?;
        decode_broadcast(value)
    }
}

fn account_body(account: Address) -> Value {
    json!({ "address": encode_tron_address(account), "visible": true })
}

/// `{}` is an account that doesn't exist; an existing one carries its address.
fn decode_account_exists(value: &Value) -> Result<bool, RpcBrokerError> {
    match value.as_object() {
        Some(account) if account.is_empty() => Ok(false),
        Some(account) if account.contains_key("address") => Ok(true),
        _ => Err(RpcBrokerError::InvalidResponse),
    }
}

/// Protobuf JSON omits zero values, so every field defaults to zero.
#[derive(Deserialize)]
struct ResourcesWire {
    #[serde(rename = "freeNetLimit", default)]
    free_net_limit: u64,
    #[serde(rename = "freeNetUsed", default)]
    free_net_used: u64,
    #[serde(rename = "NetLimit", default)]
    net_limit: u64,
    #[serde(rename = "NetUsed", default)]
    net_used: u64,
    #[serde(rename = "EnergyLimit", default)]
    energy_limit: u64,
    #[serde(rename = "EnergyUsed", default)]
    energy_used: u64,
}

fn decode_account_resources(value: Value) -> Result<AccountResources, RpcBrokerError> {
    let wire: ResourcesWire =
        serde_json::from_value(value).map_err(|_| RpcBrokerError::InvalidResponse)?;
    Ok(AccountResources {
        free_bandwidth: wire.free_net_limit.saturating_sub(wire.free_net_used),
        staked_bandwidth: wire.net_limit.saturating_sub(wire.net_used),
        energy: wire.energy_limit.saturating_sub(wire.energy_used),
    })
}

#[derive(Deserialize)]
struct ChainParametersWire {
    #[serde(rename = "chainParameter")]
    parameters: Vec<ChainParameterWire>,
}

#[derive(Deserialize)]
struct ChainParameterWire {
    key: String,
    #[serde(default)]
    value: i64,
}

fn decode_chain_fee_parameters(value: Value) -> Result<ChainFeeParameters, RpcBrokerError> {
    let wire: ChainParametersWire =
        serde_json::from_value(value).map_err(|_| RpcBrokerError::InvalidResponse)?;
    let parameter = |key: &str| {
        wire.parameters
            .iter()
            .find(|parameter| parameter.key == key)
            .and_then(|parameter| u64::try_from(parameter.value).ok())
            .ok_or(RpcBrokerError::InvalidResponse)
    };
    Ok(ChainFeeParameters {
        bandwidth_price: parameter("getTransactionFee")?,
        energy_price: parameter("getEnergyFee")?,
        create_account_fee: parameter("getCreateAccountFee")?,
        create_account_system_fee: parameter("getCreateNewAccountFeeInSystemContract")?,
        create_account_bandwidth_rate: parameter("getCreateNewAccountBandwidthRate")?,
    })
}

#[derive(Deserialize)]
struct TransactionInfoWire {
    id: B256,
    #[serde(default)]
    fee: u64,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    receipt: ReceiptWire,
    #[serde(default)]
    log: Vec<LogWire>,
}

#[derive(Default, Deserialize)]
struct ReceiptWire {
    #[serde(default)]
    result: Option<String>,
}

/// Tron gives addresses, topics and data as hex without `0x`, and addresses without `41`.
#[derive(Deserialize)]
struct LogWire {
    address: Address,
    #[serde(default)]
    topics: Vec<B256>,
    #[serde(default)]
    data: Bytes,
}

pub(super) fn decode_transaction_infos(
    value: Value,
) -> Result<Vec<TronTransactionInfo>, RpcBrokerError> {
    let wire: Vec<TransactionInfoWire> =
        serde_json::from_value(value).map_err(|_| RpcBrokerError::InvalidResponse)?;
    wire.into_iter()
        .map(|info| {
            let logs = info
                .log
                .into_iter()
                .map(|log| {
                    Log::new(log.address, log.topics, log.data)
                        .ok_or(RpcBrokerError::InvalidResponse)
                })
                .collect::<Result<_, _>>()?;
            Ok(TronTransactionInfo {
                id: info.id,
                succeeded: info.result.as_deref() != Some("FAILED")
                    && info
                        .receipt
                        .result
                        .as_deref()
                        .is_none_or(|result| result == "SUCCESS"),
                logs,
                fee_sun: info.fee,
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct BroadcastWire {
    #[serde(default)]
    result: bool,
    #[serde(default)]
    code: Option<String>,
}

fn decode_broadcast(value: Value) -> Result<TronBroadcast, RpcBrokerError> {
    let wire: BroadcastWire =
        serde_json::from_value(value).map_err(|_| RpcBrokerError::InvalidResponse)?;
    Ok(TronBroadcast {
        accepted: wire.result,
        code: wire.code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256};

    /// Mainnet responses recorded on 2026-10-31, arrays trimmed.
    fn fixture(name: &str) -> Value {
        let fixtures: Value = serde_json::from_str(include_str!("fixtures/http_api.json")).unwrap();
        fixtures[name].clone()
    }

    #[test]
    fn recorded_responses_decode() {
        assert_eq!(decode_account_exists(&fixture("getaccount")), Ok(true));
        assert_eq!(
            decode_account_exists(&fixture("getaccount_missing")),
            Ok(false)
        );
        assert_eq!(
            decode_account_resources(fixture("getaccountresource")),
            Ok(AccountResources {
                free_bandwidth: 600,
                staked_bandwidth: 3_528_933,
                energy: 1_282_988_888,
            })
        );
        assert_eq!(
            decode_chain_fee_parameters(fixture("getchainparameters")),
            Ok(ChainFeeParameters {
                bandwidth_price: 1_000,
                energy_price: 100,
                create_account_fee: 100_000,
                create_account_system_fee: 1_000_000,
                create_account_bandwidth_rate: 1,
            })
        );
        assert_eq!(
            decode_broadcast(fixture("broadcasthex_rejected")),
            Ok(TronBroadcast {
                accepted: false,
                code: Some("CONTRACT_VALIDATE_ERROR".into()),
            })
        );

        let infos =
            decode_transaction_infos(fixture("gettransactioninfobyblocknum_87594600")).unwrap();
        let [usdt, trx, reverted] = infos.as_slice() else {
            panic!("three recorded transactions");
        };
        assert_eq!(
            usdt.id,
            b256!("ade12ef05202cc1d37e47ac54d67efd58a968eba63d4248565c55d1a051ff759")
        );
        assert!(usdt.succeeded);
        let [transfer] = usdt.logs.as_slice() else {
            panic!("one Transfer log");
        };
        // USDT, `TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t`.
        assert_eq!(
            transfer.address,
            address!("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c")
        );
        assert_eq!(transfer.data.topics().len(), 3);
        assert_eq!(transfer.data.data.len(), 32);
        assert!(trx.succeeded && trx.logs.is_empty());
        assert!(!reverted.succeeded);
    }
}
