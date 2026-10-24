use std::time::Instant;

use alloy::primitives::{U256, address};
use alloy::providers::Provider as _;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall as _;
use eyre::{Result, eyre};

use crate::rpc_broker::RpcCandidates;
use crate::settings::EffectiveChainGasSettings;

alloy::sol! {
    // Nitro's public price oracle. Alloy has no built-in binding for this precompile.
    // https://github.com/OffchainLabs/nitro-precompile-interfaces/blob/main/ArbGasInfo.sol
    interface ArbGasInfo {
        function getL1BaseFeeEstimate() external view returns (uint256);
    }
}

/// Price the additional hook calldata without exposing hooks, notes, or account addresses.
/// The hook-free `CoW` quote already covers the rest of the settlement's data cost.
pub(super) async fn hook_data_cost_from_candidates(
    candidates: &RpcCandidates,
    chain_id: u64,
    app_data_len: usize,
    gas: &EffectiveChainGasSettings,
) -> Result<U256> {
    if chain_id != 42161 {
        return Ok(U256::ZERO);
    }
    let started = Instant::now();
    tracing::debug!(target: "swap_quote", step = "hook_data_cost", "started");
    let result = async {
        for _ in 0..candidates.len() {
            let Some(handle) = candidates.random_ready().await else {
                break;
            };
            let response = handle
                .provider
                .call(
                    TransactionRequest::default()
                        .to(address!("000000000000000000000000000000000000006C"))
                        .input(ArbGasInfo::getL1BaseFeeEstimateCall {}.abi_encode().into()),
                )
                .await;
            if let Some(price) = response.ok().and_then(|bytes| {
                ArbGasInfo::getL1BaseFeeEstimateCall::abi_decode_returns(&bytes).ok()
            }) {
                // Hex in the app data uses two characters per hook byte. Keep the JSON
                // overhead too, plus 1 KiB for both trampoline/settlement ABI wrappers.
                // No compression discount: proofs and ciphertexts compress poorly.
                let bytes = U256::from(app_data_len.div_ceil(2)) + U256::from(1024);
                let cost = price
                    .checked_mul(U256::from(16))
                    .and_then(|price| price.checked_mul(bytes))
                    .and_then(|cost| cost.checked_mul(U256::from(gas.gas_price_buffer_numerator)))
                    .ok_or_else(|| eyre!("Arbitrum hook data cost overflow"))?;
                return Ok(cost.div_ceil(U256::from(gas.gas_price_buffer_denominator)));
            }
            tracing::warn!(rpc_index = handle.index, "fetch Arbitrum data price failed");
            candidates.pool().mark_bad_provider(&handle);
        }
        Err(eyre!("Could not estimate Arbitrum data costs. Try again."))
    }
    .await;
    tracing::debug!(
        target: "swap_quote",
        step = "hook_data_cost",
        elapsed_ms = started.elapsed().as_millis(),
        success = result.is_ok(),
        "finished"
    );
    result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy::primitives::{Bytes, U64};
    use alloy::sol_types::SolValue as _;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;

    use super::*;
    use crate::rpc_broker::RpcChainRoute;
    use crate::rpc_broker::tests::{RpcMockGate, spawn_gated_rpc_mock, spawn_rpc_mock};

    #[tokio::test]
    async fn rollup_data_cost_reads_only_the_public_oracle_and_fails_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url: url::Url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let http = crate::HttpContext::direct_for_tests();
        let candidates = RpcCandidates::acquire(&http, &RpcChainRoute::new(42161, vec![url]), None)
            .await
            .unwrap();
        let gas = EffectiveChainGasSettings {
            gas_limit_buffer: 0,
            gas_price_buffer_numerator: 3,
            gas_price_buffer_denominator: 2,
        };
        let serve = tokio::spawn(async move {
            // A valid oracle result, then an unavailable oracle. Other chains make no call.
            for result in [
                json!({"result": Bytes::from(U256::from(100).abi_encode())}),
                json!({"error": {"code": -32000, "message": "unavailable"}}),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let request = loop {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n")
                        && let Ok(value) =
                            serde_json::from_slice::<serde_json::Value>(&bytes[end + 4..])
                    {
                        break value;
                    }
                };
                assert_eq!(request["method"], "eth_call");
                let call: TransactionRequest =
                    serde_json::from_value(request["params"][0].clone()).unwrap();
                assert_eq!(
                    call.to,
                    Some(address!("000000000000000000000000000000000000006C").into())
                );
                assert!(call.from.is_none());
                assert_eq!(
                    call.input.input().unwrap().as_ref(),
                    ArbGasInfo::getL1BaseFeeEstimateCall {}.abi_encode()
                );
                let mut response = result;
                response["jsonrpc"] = json!("2.0");
                response["id"] = request["id"].clone();
                let body = response.to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        for chain in [1, 56, 137] {
            assert_eq!(
                hook_data_cost_from_candidates(&candidates, chain, 8000, &gas)
                    .await
                    .unwrap(),
                U256::ZERO
            );
        }
        // 4,000 hook/metadata bytes + 1,024 ABI overhead; 100 wei per L1 gas,
        // 16 gas per byte, buffered by 3/2. No private payload is sent to the oracle.
        assert_eq!(
            hook_data_cost_from_candidates(&candidates, 42161, 8000, &gas)
                .await
                .unwrap(),
            U256::from(12_057_600)
        );
        assert!(
            hook_data_cost_from_candidates(&candidates, 42161, 8000, &gas)
                .await
                .is_err()
        );
        serve.await.unwrap();
    }

    /// An Arbitrum oracle endpoint answering `eth_chainId` with Arbitrum One and `eth_call` with
    /// `result`, notifying `called` on each oracle call.
    fn oracle(result: Value, called: Arc<Notify>) -> crate::rpc_broker::tests::RpcResponder {
        Arc::new(move |request: Value| {
            let result = if request["method"] == "eth_chainId" {
                json!(U64::from(42161))
            } else {
                called.notify_one();
                result.clone()
            };
            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        })
    }

    #[tokio::test]
    async fn rollup_data_cost_retries_an_undecodable_oracle_on_a_later_admitted_endpoint() {
        let gas = EffectiveChainGasSettings {
            gas_limit_buffer: 0,
            gas_price_buffer_numerator: 3,
            gas_price_buffer_denominator: 2,
        };
        let undecodable_called = Arc::new(Notify::new());
        let (undecodable, undecodable_server) = spawn_rpc_mock(
            oracle(json!("0x"), undecodable_called.clone()),
            Arc::default(),
            Arc::default(),
        )
        .await;
        let gate = RpcMockGate::default();
        let (valid, valid_server) = spawn_gated_rpc_mock(
            oracle(
                json!(Bytes::from(U256::from(100).abi_encode())),
                Arc::default(),
            ),
            Arc::default(),
            Arc::default(),
            gate.clone(),
        )
        .await;
        let http = crate::HttpContext::direct_for_tests();
        let route = RpcChainRoute::new(42161, vec![undecodable.clone(), valid])
            .with_identity_verification();
        route
            .for_endpoint(undecodable.into())
            .verify_identity(&http.rpc_clients())
            .await
            .expect("a clone verifies the undecodable endpoint");
        let candidates = RpcCandidates::acquire(&http, &route, None)
            .await
            .expect("candidates");
        let cost = tokio::spawn(async move {
            hook_data_cost_from_candidates(&candidates, 42161, 8000, &gas).await
        });

        // Only the verified endpoint is ready, and its answer can't be decoded. The search then
        // waits for the other endpoint's identity check instead of failing.
        gate.request_started.notified().await;
        undecodable_called.notified().await;
        gate.release_response.notify_one();
        gate.request_started.notified().await;
        gate.release_response.notify_one();
        assert_eq!(
            cost.await.expect("data cost task").expect("data cost"),
            U256::from(12_057_600)
        );
        undecodable_server.abort();
        valid_server.abort();
    }
}
