//! Broker submission for reads admitted by the gateway owner.

use super::errors::{ProviderAvailability, ProviderRpcError};
use crate::rpc_broker::{
    RpcBrokerError, RpcChainRoute, RpcOrigin, RpcRead, RpcResult, RpcRoute, RpcSubmission,
    TransactionHashRead, WalletRpcOrigin,
};
use crate::{HttpContext, PublicTransactionLookup};
use alloy::eips::{BlockNumHash, BlockNumberOrTag};
use alloy::network::primitives::HeaderResponse;
use alloy::network::{
    AnyRpcBlock, AnyRpcTransaction, AnyTransactionReceipt, BlockResponse, ReceiptResponse,
    TransactionResponse,
};
use alloy::primitives::{B256, U256};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use tokio::time::Instant;

/// Uses the broker's Alloy ingress without adding protocol acceptance rules.
pub(super) fn parse_read(
    method: &str,
    params: Value,
    chain_id: u64,
) -> Result<RpcRead, ProviderRpcError> {
    RpcRead::from_method_params(method, params, chain_id).map_err(|error| {
        ProviderRpcError::from_broker(
            RpcBrokerError::InvalidRead(error),
            ProviderAvailability::Available,
        )
    })
}

/// Keeps fixed observation failures distinct from owner-bound broker payloads.
#[derive(Debug)]
pub(super) enum ReadError {
    Broker(RpcBrokerError),
    Unavailable,
}
impl From<RpcBrokerError> for ReadError {
    fn from(error: RpcBrokerError) -> Self {
        Self::Broker(error)
    }
}

pub(super) async fn submit_read(
    http: HttpContext,
    chain: RpcChainRoute,
    origin: RpcOrigin,
    read: RpcRead,
    lookup: Option<(TransactionHashRead, B256, PublicTransactionLookup)>,
    deadline: Instant,
) -> Result<Value, ReadError> {
    if Instant::now() >= deadline {
        return Err(ReadError::Unavailable);
    }
    if let Some((kind, hash, lookup)) = lookup {
        match lookup {
            PublicTransactionLookup::Pending => return Ok(Value::Null),
            PublicTransactionLookup::Unavailable => return Err(ReadError::Unavailable),
            PublicTransactionLookup::Included {
                block,
                transaction_index,
            } => {
                return included_read(http, chain, kind, hash, block, transaction_index, deadline)
                    .await;
            }
            PublicTransactionLookup::Untracked | PublicTransactionLookup::DiscoveryExhausted => {}
        }
    }
    let mut members = submit_reads(http, chain, origin, vec![read], deadline).await?;
    if members.len() != 1 {
        return Err(RpcBrokerError::InvalidResponse.into());
    }
    members
        .remove(0)
        .map(RpcResult::into_value)
        .map_err(Into::into)
}

async fn included_read(
    http: HttpContext,
    chain: RpcChainRoute,
    kind: TransactionHashRead,
    hash: B256,
    block: BlockNumHash,
    transaction_index: u64,
    deadline: Instant,
) -> Result<Value, ReadError> {
    let included = match kind {
        TransactionHashRead::Transaction => RpcRead::get_transaction_by_block_hash_and_index(
            block.hash,
            U256::from(transaction_index),
        ),
        TransactionHashRead::Receipt => RpcRead::get_block_receipts_by_hash(block.hash),
    };
    let reads = vec![
        RpcRead::get_block_by_number(BlockNumberOrTag::Number(block.number), false),
        included,
    ];
    // Both reads enter the same accepted job. No new submission follows an owner invalidation.
    let members = submit_reads(
        http,
        chain,
        WalletRpcOrigin::PublicWallet.into(),
        reads,
        deadline,
    )
    .await?;
    if members.len() != 2 {
        return Err(RpcBrokerError::InvalidResponse.into());
    }
    let mut members = members.into_iter();
    let canonical = members.next().expect("two members")?;
    let canonical =
        AnyRpcBlock::deserialize(canonical.expose_value()).map_err(|_| ReadError::Unavailable)?;
    if canonical.header().num_hash() != block {
        return Err(ReadError::Unavailable);
    }
    let value = members.next().expect("two members")?.into_value();
    match kind {
        TransactionHashRead::Transaction => {
            let transaction =
                AnyRpcTransaction::deserialize(&value).map_err(|_| ReadError::Unavailable)?;
            if transaction.tx_hash() != hash
                || transaction.block_hash_num() != Some(block)
                || transaction.transaction_index() != Some(transaction_index)
            {
                return Err(ReadError::Unavailable);
            }
            Ok(value)
        }
        TransactionHashRead::Receipt => {
            let receipts = value.as_array().ok_or(ReadError::Unavailable)?;
            let mut selected = None;
            for receipt in receipts {
                // Match the broker's pinned Alloy receipt acceptance, including its Ethereum fallback.
                let identity = AnyTransactionReceipt::deserialize(receipt)
                    .map(|receipt| receipt_identity(&receipt))
                    .or_else(|_| {
                        alloy::rpc::types::TransactionReceipt::deserialize(receipt).map(
                            |receipt: alloy::rpc::types::TransactionReceipt| {
                                receipt_identity(&receipt)
                            },
                        )
                    })
                    .map_err(|_| ReadError::Unavailable)?;
                if identity.1 != Some(block) {
                    return Err(ReadError::Unavailable);
                }
                if identity.0 == hash {
                    if identity.2 != Some(transaction_index) || selected.is_some() {
                        return Err(ReadError::Unavailable);
                    }
                    selected = Some(receipt.clone());
                }
            }
            selected.ok_or(ReadError::Unavailable)
        }
    }
}

fn receipt_identity(receipt: &impl ReceiptResponse) -> (B256, Option<BlockNumHash>, Option<u64>) {
    (
        receipt.transaction_hash(),
        receipt.block_hash_num(),
        receipt.transaction_index(),
    )
}

/// Preserves ordered member results and outer submission failures until owner revalidation.
///
/// The caller retains admission charges and drains this future after invalidation, then checks
/// its original deadline and response owner before exposing either successful or error payloads.
pub(super) async fn submit_reads(
    http: HttpContext,
    chain: RpcChainRoute,
    origin: RpcOrigin,
    reads: Vec<RpcRead>,
    deadline: Instant,
) -> Result<Vec<Result<RpcResult, RpcBrokerError>>, RpcBrokerError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(RpcBrokerError::TimeoutBeforeDispatch);
    }
    let route = RpcRoute::from(chain)
        .with_request_timeout(remaining)
        .with_attempt_timeout(Duration::from_secs(10));
    http.rpc_broker()
        .submit(RpcSubmission::new(route, reads, origin))
        .await
}

#[cfg(test)]
mod tests;
