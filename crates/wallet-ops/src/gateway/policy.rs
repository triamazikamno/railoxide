//! Token review metadata uses the requesting origin's admitted reads.
use crate::rpc_broker::RpcBrokerViewCalls;
use crate::settings::CustomTokenSettings;
use crate::{DappRpcReadClient, RpcBrokerError, SensitiveUrl};
use alloy::{
    primitives::{Address, Bytes},
    sol_types::SolCall,
};

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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

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
