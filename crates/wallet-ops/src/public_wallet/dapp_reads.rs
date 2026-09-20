use std::sync::Arc;

use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes, U64, U128, U256};
use alloy::rpc::types::{BlockNumberOrTag, FeeHistory, TransactionRequest};
use alloy::serde::WithOtherFields;
use futures_util::future::BoxFuture;
use poi::SensitiveUrl;
use serde::de::DeserializeOwned;
use serde_json::{Number, Value};

use crate::rpc_broker::{RpcBrokerError, RpcRead, RpcReadValidationError};

type ReadCallback = dyn Fn(SensitiveUrl, RpcRead) -> BoxFuture<'static, Result<Value, RpcBrokerError>>
    + Send
    + Sync;

/// Supplies one separately admitted RPC operation per logical dapp read.
/// The caller owns admission and the lifetime of accepted operations.
#[derive(Clone)]
pub struct DappRpcReadClient {
    callback: Arc<ReadCallback>,
}

impl DappRpcReadClient {
    #[must_use]
    pub fn new(
        callback: impl Fn(SensitiveUrl, RpcRead) -> BoxFuture<'static, Result<Value, RpcBrokerError>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            callback: Arc::new(callback),
        }
    }

    pub async fn read(
        &self,
        endpoint: SensitiveUrl,
        read: RpcRead,
    ) -> Result<Value, RpcBrokerError> {
        (self.callback)(endpoint, read).await
    }

    async fn read_typed<T: DeserializeOwned>(
        &self,
        endpoint: SensitiveUrl,
        chain_id: u64,
        read: RpcRead,
    ) -> Result<T, RpcBrokerError> {
        let method = read.method();
        let value = self.read(endpoint, read).await.inspect_err(|error| {
            tracing::warn!(
                rpc_method = method,
                chain_id,
                ?error,
                "dapp approval RPC read failed"
            );
        })?;
        serde_json::from_value(value).map_err(|_| RpcBrokerError::InvalidResponse)
    }

    pub(crate) async fn eth_call(
        &self,
        endpoint: SensitiveUrl,
        chain_id: u64,
        target: Address,
        calldata: Bytes,
    ) -> Result<Bytes, RpcBrokerError> {
        self.read_typed(endpoint, chain_id, RpcRead::eth_call(target, calldata))
            .await
    }

    pub(crate) async fn get_balance(
        &self,
        endpoint: SensitiveUrl,
        chain_id: u64,
        account: Address,
    ) -> Result<U256, RpcBrokerError> {
        self.read_typed(endpoint, chain_id, RpcRead::get_balance(account))
            .await
    }

    pub(crate) async fn get_transaction_count(
        &self,
        endpoint: SensitiveUrl,
        chain_id: u64,
        account: Address,
    ) -> Result<u64, RpcBrokerError> {
        self.read_typed::<U64>(
            endpoint,
            chain_id,
            RpcRead::get_transaction_count(account, BlockId::latest()),
        )
        .await
        .map(|value| value.to())
    }

    pub(crate) async fn get_gas_price(
        &self,
        endpoint: SensitiveUrl,
        chain_id: u64,
    ) -> Result<u128, RpcBrokerError> {
        self.read_typed::<U128>(endpoint, chain_id, RpcRead::gas_price())
            .await
            .map(|value| value.to())
    }

    pub(crate) async fn latest_block(
        &self,
        endpoint: SensitiveUrl,
        chain_id: u64,
    ) -> Result<Option<alloy::rpc::types::Block>, RpcBrokerError> {
        self.read_typed(
            endpoint,
            chain_id,
            RpcRead::get_block_by_number(BlockNumberOrTag::Latest, false),
        )
        .await
    }

    pub(crate) async fn get_max_priority_fee_per_gas(
        &self,
        endpoint: SensitiveUrl,
        chain_id: u64,
    ) -> Result<u128, RpcBrokerError> {
        self.read_typed::<U128>(endpoint, chain_id, RpcRead::max_priority_fee_per_gas())
            .await
            .map(|value| value.to())
    }

    pub(crate) async fn get_fee_history(
        &self,
        endpoint: SensitiveUrl,
        chain_id: u64,
        count: u64,
        newest: BlockNumberOrTag,
        percentiles: &[f64],
    ) -> Result<FeeHistory, RpcBrokerError> {
        let percentiles = percentiles
            .iter()
            .map(|value| Number::from_f64(*value))
            .collect::<Option<Vec<_>>>()
            .ok_or(RpcBrokerError::InvalidRead(
                RpcReadValidationError::InvalidParams,
            ))?;
        self.read_typed(
            endpoint,
            chain_id,
            RpcRead::fee_history(U256::from(count), newest, Some(percentiles)),
        )
        .await
    }

    pub(crate) async fn estimate_gas(
        &self,
        endpoint: SensitiveUrl,
        chain_id: u64,
        tx: &TransactionRequest,
    ) -> Result<u64, RpcBrokerError> {
        // Match Alloy Provider::estimate_gas, which explicitly selects pending.
        let read = RpcRead::estimate_gas(
            WithOtherFields::new(tx.clone()),
            Some(BlockNumberOrTag::Pending),
            chain_id,
        )
        .map_err(RpcBrokerError::InvalidRead)?;
        self.read_typed::<U64>(endpoint, chain_id, read)
            .await
            .map(|value| value.to())
    }
}

pub(crate) const fn stops_dapp_read_retries(error: &RpcBrokerError) -> bool {
    matches!(
        error,
        RpcBrokerError::OriginRejected
            | RpcBrokerError::AdmissionRejected
            | RpcBrokerError::TimeoutBeforeDispatch
            | RpcBrokerError::Shutdown
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::rpc::types::TransactionInput;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn gas_estimation_validates_before_dispatch_and_preserves_pending_request() {
        let tx = TransactionRequest {
            from: Some(Address::from([1; 20])),
            to: Some(Address::from([2; 20]).into()),
            value: Some(U256::from(7)),
            input: TransactionInput::new(Bytes::from_static(b"\x01\x02")),
            chain_id: Some(1),
            ..TransactionRequest::default()
        };
        let expected =
            RpcRead::from_method_params("eth_estimateGas", json!([tx, "pending"]), 1).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let reads = DappRpcReadClient::new({
            let calls = Arc::clone(&calls);
            move |_, read| {
                assert_eq!(read, expected);
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(json!("0x5208")) })
            }
        });
        let endpoint: SensitiveUrl = url::Url::parse("http://127.0.0.1:1").unwrap().into();
        assert_eq!(
            reads.estimate_gas(endpoint.clone(), 2, &tx).await,
            Err(RpcBrokerError::InvalidRead(
                RpcReadValidationError::ChainIdMismatch
            ))
        );
        let conflicting = TransactionRequest {
            input: TransactionInput {
                input: Some(Bytes::from_static(b"\x01")),
                data: Some(Bytes::from_static(b"\x02")),
            },
            ..tx.clone()
        };
        assert_eq!(
            reads.estimate_gas(endpoint.clone(), 1, &conflicting).await,
            Err(RpcBrokerError::InvalidRead(
                RpcReadValidationError::InvalidParams
            ))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(reads.estimate_gas(endpoint, 1, &tx).await.unwrap(), 21_000);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
