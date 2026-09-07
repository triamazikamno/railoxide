use super::model::{RpcBrokerError, RpcRead, RpcReadValidationError, RpcResult};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::AnyNetwork;
use alloy::network::{AnyRpcBlock, AnyRpcTransaction, AnyTransactionReceipt};
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::EthCallParams;
use alloy::rpc::types::eth::state::StateOverride;
use alloy::rpc::types::{FeeHistory, Filter, Log, TransactionReceipt, TransactionRequest};
use alloy::serde::WithOtherFields;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Number, Value, json};
use std::sync::Arc;

/// Validated typed parameters for every supported broker operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RpcOperation {
    EthCall {
        request: Arc<WithOtherFields<TransactionRequest>>,
        state_overrides: Option<StateOverride>,
        block: BlockId,
    },
    GetBalance {
        account: Address,
        block: BlockId,
    },
    BlockNumber,
    GetCode(Address, BlockId),
    GetStorageAt(Address, U256, BlockId),
    GetTransactionCount(Address, BlockId),
    GetBlockByHash(B256, bool),
    GetBlockByNumber(BlockNumberOrTag, bool),
    GetBlockTransactionCountByHash(B256),
    GetBlockTransactionCountByNumber(BlockNumberOrTag),
    GetTransactionByHash(B256),
    GetTransactionReceipt(B256),
    GetTransactionByBlockHashAndIndex(B256, U256),
    GetTransactionByBlockNumberAndIndex(BlockNumberOrTag, U256),
    GetBlockReceipts(ReceiptBlock),
    GetLogs(Box<Filter>, Option<usize>),
    GasPrice,
    MaxPriorityFeePerGas,
    FeeHistory(U256, BlockNumberOrTag, Option<Vec<Number>>),
    EstimateGas(
        Arc<WithOtherFields<TransactionRequest>>,
        Option<BlockNumberOrTag>,
    ),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub(super) enum ReceiptBlock {
    Hash(B256),
    Number(BlockNumberOrTag),
}

pub(super) fn typed<T: DeserializeOwned>(value: Value) -> Result<T, RpcReadValidationError> {
    serde_json::from_value(value).map_err(|_| RpcReadValidationError::InvalidParams)
}

// Account for the original alternatives before Alloy normalizes and deduplicates them.
fn filter_input_size(value: &Value) -> Option<usize> {
    let alternatives = |value: &Value| {
        if value.is_null() {
            0
        } else {
            value.as_array().map_or(1, Vec::len)
        }
    };
    let mut size = value
        .get("address")
        .map_or(0, alternatives)
        .checked_mul(20)?;
    if let Some(topics) = value.get("topics").and_then(Value::as_array) {
        for topic in topics {
            size = size.checked_add(1)?;
            size = size.checked_add(alternatives(topic).checked_mul(32)?)?;
        }
    }
    Some(size)
}

impl RpcOperation {
    pub(super) fn parse(
        method: &str,
        params: Vec<Value>,
        chain_id: u64,
    ) -> Result<Self, RpcReadValidationError> {
        let invalid = RpcReadValidationError::InvalidParams;
        let count = params.len();
        let mut params = params.into_iter();
        let mut next = || params.next().ok_or(RpcReadValidationError::InvalidParams);
        Ok(match (method, count) {
            ("eth_call", 1..=3) => {
                return RpcRead::from_rpc_params(Value::Array(params.collect()), chain_id)
                    .map(|read| read.operation().clone());
            }
            ("eth_getBalance", 2) => Self::GetBalance {
                account: typed(next()?)?,
                block: typed(next()?)?,
            },
            ("eth_blockNumber", 0) => Self::BlockNumber,
            ("eth_getCode", 2) => Self::GetCode(typed(next()?)?, typed(next()?)?),
            ("eth_getStorageAt", 3) => {
                Self::GetStorageAt(typed(next()?)?, typed(next()?)?, typed(next()?)?)
            }
            ("eth_getTransactionCount", 2) => {
                Self::GetTransactionCount(typed(next()?)?, typed(next()?)?)
            }
            ("eth_getBlockByHash", 2) => Self::GetBlockByHash(typed(next()?)?, typed(next()?)?),
            ("eth_getBlockByNumber", 2) => Self::GetBlockByNumber(typed(next()?)?, typed(next()?)?),
            ("eth_getBlockTransactionCountByHash", 1) => {
                Self::GetBlockTransactionCountByHash(typed(next()?)?)
            }
            ("eth_getBlockTransactionCountByNumber", 1) => {
                Self::GetBlockTransactionCountByNumber(typed(next()?)?)
            }
            ("eth_getTransactionByHash", 1) => Self::GetTransactionByHash(typed(next()?)?),
            ("eth_getTransactionReceipt", 1) => Self::GetTransactionReceipt(typed(next()?)?),
            ("eth_getTransactionByBlockHashAndIndex", 2) => {
                Self::GetTransactionByBlockHashAndIndex(typed(next()?)?, typed(next()?)?)
            }
            ("eth_getTransactionByBlockNumberAndIndex", 2) => {
                Self::GetTransactionByBlockNumberAndIndex(typed(next()?)?, typed(next()?)?)
            }
            ("eth_getBlockReceipts", 1) => Self::GetBlockReceipts(typed(next()?)?),
            ("eth_getLogs", 1) => {
                let value = next()?;
                let input_size = filter_input_size(&value);
                Self::GetLogs(typed(value)?, input_size)
            }
            ("eth_gasPrice", 0) => Self::GasPrice,
            ("eth_maxPriorityFeePerGas", 0) => Self::MaxPriorityFeePerGas,
            ("eth_feeHistory", 2 | 3) => {
                let count = typed(next()?)?;
                let newest = typed(next()?)?;
                let percentiles: Option<Vec<Number>> = params.next().map(typed).transpose()?;
                Self::FeeHistory(count, newest, percentiles)
            }
            ("eth_estimateGas", 1 | 2) => {
                let transaction = next()?;
                let read =
                    RpcRead::from_rpc(typed(transaction)?, BlockId::latest(), None, chain_id)?;
                let Self::EthCall { request, .. } = read.operation() else {
                    unreachable!()
                };
                Self::EstimateGas(Arc::clone(request), params.next().map(typed).transpose()?)
            }
            _ => return Err(invalid),
        })
    }

    pub(super) const fn exact_transaction_hash(&self) -> bool {
        matches!(
            self,
            Self::GetTransactionByHash(_) | Self::GetTransactionReceipt(_)
        )
    }

    pub(super) fn decoded_input_size(&self) -> Option<usize> {
        match self {
            Self::GetLogs(_, input_size) => *input_size,
            Self::FeeHistory(_, _, percentiles) => {
                percentiles.as_ref().map_or(0, Vec::len).checked_mul(8)
            }
            Self::EstimateGas(request, _) => super::model::transaction_input_size(request),
            _ => Some(0),
        }
    }

    pub(super) fn wire(&self) -> (&'static str, Value, bool) {
        let (method, params) = match self {
            Self::EthCall {
                request,
                state_overrides,
                block,
            } => {
                let params = EthCallParams::<AnyNetwork>::new((**request).clone())
                    .with_block(*block)
                    .with_overrides_opt(state_overrides.clone());
                (
                    "eth_call",
                    serde_json::to_value(params).expect("Alloy eth_call params serialize"),
                )
            }
            Self::GetBalance { account, block } => ("eth_getBalance", json!([account, block])),
            Self::BlockNumber => ("eth_blockNumber", json!([])),
            Self::GetCode(address, block) => ("eth_getCode", json!([address, block])),
            Self::GetStorageAt(address, slot, block) => {
                ("eth_getStorageAt", json!([address, slot, block]))
            }
            Self::GetTransactionCount(address, block) => {
                ("eth_getTransactionCount", json!([address, block]))
            }
            Self::GetBlockByHash(hash, full) => ("eth_getBlockByHash", json!([hash, full])),
            Self::GetBlockByNumber(block, full) => ("eth_getBlockByNumber", json!([block, full])),
            Self::GetBlockTransactionCountByHash(hash) => {
                ("eth_getBlockTransactionCountByHash", json!([hash]))
            }
            Self::GetBlockTransactionCountByNumber(block) => {
                ("eth_getBlockTransactionCountByNumber", json!([block]))
            }
            Self::GetTransactionByHash(hash) => ("eth_getTransactionByHash", json!([hash])),
            Self::GetTransactionReceipt(hash) => ("eth_getTransactionReceipt", json!([hash])),
            Self::GetTransactionByBlockHashAndIndex(hash, index) => (
                "eth_getTransactionByBlockHashAndIndex",
                json!([hash, index]),
            ),
            Self::GetTransactionByBlockNumberAndIndex(block, index) => (
                "eth_getTransactionByBlockNumberAndIndex",
                json!([block, index]),
            ),
            Self::GetBlockReceipts(block) => ("eth_getBlockReceipts", json!([block])),
            Self::GetLogs(filter, _) => ("eth_getLogs", json!([filter])),
            Self::GasPrice => ("eth_gasPrice", json!([])),
            Self::MaxPriorityFeePerGas => ("eth_maxPriorityFeePerGas", json!([])),
            Self::FeeHistory(count, newest, percentiles) => (
                "eth_feeHistory",
                match percentiles {
                    Some(percentiles) => json!([count, newest, percentiles]),
                    None => json!([count, newest]),
                },
            ),
            Self::EstimateGas(request, block) => (
                "eth_estimateGas",
                match block {
                    Some(block) => json!([request.as_ref(), block]),
                    None => json!([request.as_ref()]),
                },
            ),
        };
        (
            method,
            params,
            matches!(self, Self::EthCall { .. } | Self::EstimateGas(..)),
        )
    }

    pub(super) fn validate_result(&self, value: Value) -> Result<RpcResult, RpcBrokerError> {
        let nullable = matches!(
            self,
            Self::GetBlockByHash(..)
                | Self::GetBlockByNumber(..)
                | Self::GetBlockTransactionCountByHash(..)
                | Self::GetBlockTransactionCountByNumber(..)
                | Self::GetTransactionByHash(..)
                | Self::GetTransactionReceipt(..)
                | Self::GetTransactionByBlockHashAndIndex(..)
                | Self::GetTransactionByBlockNumberAndIndex(..)
                | Self::GetBlockReceipts(..)
        );
        if nullable && value.is_null() {
            return Ok(RpcResult::new(value));
        }
        match self {
            Self::EthCall { .. } | Self::GetCode(..) => validate_as::<Bytes>(&value)?,
            Self::GetStorageAt(..) => validate_as::<B256>(&value)?,
            Self::GetBlockByHash(..) | Self::GetBlockByNumber(..) => {
                let mut validation_value = value.clone();
                if matches!(self, Self::GetBlockByNumber(BlockNumberOrTag::Pending, _)) {
                    for (field, placeholder) in
                        [("hash", json!(B256::ZERO)), ("miner", json!(Address::ZERO))]
                    {
                        if validation_value.get(field).is_some_and(Value::is_null) {
                            validation_value[field] = placeholder;
                        }
                    }
                }
                validate_as::<AnyRpcBlock>(&validation_value)?;
            }
            Self::GetTransactionByHash(..)
            | Self::GetTransactionByBlockHashAndIndex(..)
            | Self::GetTransactionByBlockNumberAndIndex(..) => {
                validate_as::<AnyRpcTransaction>(&value)?;
            }
            Self::GetTransactionReceipt(..) => validate_receipt(&value)?,
            Self::GetBlockReceipts(..) => {
                for receipt in value.as_array().ok_or(RpcBrokerError::InvalidResponse)? {
                    validate_receipt(receipt)?;
                }
            }
            Self::GetLogs(..) => validate_as::<Vec<Log>>(&value)?,
            Self::FeeHistory(..) => validate_as::<FeeHistory>(&value)?,
            _ => validate_as::<U256>(&value)?,
        }
        Ok(RpcResult::new(value))
    }
}

fn validate_as<T: DeserializeOwned>(value: &Value) -> Result<(), RpcBrokerError> {
    T::deserialize(value)
        .map(|_| ())
        .map_err(|_| RpcBrokerError::InvalidResponse)
}

fn validate_receipt(value: &Value) -> Result<(), RpcBrokerError> {
    validate_as::<AnyTransactionReceipt>(value)
        .or_else(|_| validate_as::<TransactionReceipt>(value))
}
