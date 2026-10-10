//! Diagnoses why the orderbook rejected an order as unfunded.
//!
//! `CoW`'s orderbook runs pre-hooks through its `HooksTrampoline`, which ignores a hook that
//! reverts, and then checks that the owner can transfer the sell token. A failing pre-hook
//! therefore comes back as an unfunded account, without its reason. Calling the pre-hook from
//! the trampoline after that rejection shows whether it fails and why.
//!
//! On Arbitrum the simulation checks that the pre-hook executes but doesn't validate its
//! declared gas limit.

use alloy::primitives::{Address, Bytes};
use alloy::providers::{DynProvider, Provider as _};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{Panic, Revert, SolError as _, decode_revert_reason};
use alloy::transports::TransportError;
use tracing::Instrument as _;

use crate::desktop::executor_observation::ObservationEndpoints;

/// Arbitrum charges L1 posting gas inside the call's budget, so a top-level cap can't model the
/// hook's execution limit.
const ARBITRUM_ONE: u64 = 42161;

/// What the signed pre-hook does when the trampoline calls it at the latest block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PreHookSimulation {
    Succeeded,
    /// A reason to show the user, never raw RPC text.
    Failed(String),
    /// No endpoint answered, or an uncapped Arbitrum call ran out of gas.
    Unavailable,
}

/// Call the signed pre-hook as the trampoline does, on the executor's admitted endpoints.
/// The trampoline forwards `gas_limit` to execution; the call adds the top-level cost the
/// settlement transaction pays instead. On Arbitrum the call is uncapped, so it shows an
/// execution failure but not whether `gas_limit` suffices.
pub(super) async fn simulate_pre_hook(
    endpoints: &ObservationEndpoints,
    trampoline: Address,
    executor: Address,
    pre_hook: &Bytes,
    gas_limit: u64,
    chain_id: u64,
) -> PreHookSimulation {
    let call = TransactionRequest::default()
        .from(trampoline)
        .to(executor)
        .input(pre_hook.clone().into());
    let capped =
        (chain_id != ARBITRUM_ONE).then(|| gas_limit.saturating_add(top_level_call_gas(pre_hook)));
    for provider in endpoints.providers().await {
        let span = tracing::debug_span!(target: "executor_observation", "endpoint", rpc_index = provider.index);
        match simulate_at(&provider.provider, &call, capped, gas_limit)
            .instrument(span)
            .await
        {
            Ok(simulation) => {
                endpoints.succeeded(&provider);
                return simulation;
            }
            Err(error) => endpoints.failed(&provider, &error.into()),
        }
    }
    PreHookSimulation::Unavailable
}

/// Gas a top-level call pays before execution: the base transaction cost and its calldata,
/// 4 gas per zero byte and 16 per other byte.
fn top_level_call_gas(calldata: &[u8]) -> u64 {
    calldata.iter().fold(21_000, |gas, byte| {
        gas.saturating_add(if *byte == 0 { 4 } else { 16 })
    })
}

/// Simulate on one endpoint with `capped` as the call's limit, or with no limit on Arbitrum.
/// An error is the endpoint's, not the pre-hook's.
async fn simulate_at(
    provider: &DynProvider,
    call: &TransactionRequest,
    capped: Option<u64>,
    declared_gas_limit: u64,
) -> Result<PreHookSimulation, TransportError> {
    let Some(gas) = capped else {
        return Ok(match execute(provider, call.clone()).await? {
            Execution::Succeeded => PreHookSimulation::Succeeded,
            Execution::Reverted(reason) => PreHookSimulation::Failed(
                reason.unwrap_or_else(|| "it reverted without a reason".to_owned()),
            ),
            // The budget also pays for L1 posting, so running out says nothing about the hook.
            Execution::OutOfGas => PreHookSimulation::Unavailable,
        });
    };
    let reason = match execute(provider, call.clone().gas_limit(gas)).await? {
        Execution::Succeeded => return Ok(PreHookSimulation::Succeeded),
        Execution::Reverted(Some(reason)) => reason,
        // Without a reason, a pre-hook that only exceeds its declared gas succeeds uncapped.
        Execution::Reverted(None) | Execution::OutOfGas => {
            match execute(provider, call.clone()).await {
                Ok(Execution::Succeeded) => {
                    format!("it needs more gas than its {declared_gas_limit} gas limit")
                }
                Ok(Execution::Reverted(Some(reason))) => reason,
                Ok(Execution::Reverted(None) | Execution::OutOfGas) | Err(_) => {
                    "it reverted without a reason".to_owned()
                }
            }
        }
    };
    Ok(PreHookSimulation::Failed(reason))
}

pub(super) enum Execution {
    Succeeded,
    /// With the decoded revert reason, when there is one.
    Reverted(Option<String>),
    /// The call's gas budget ran out, before or during execution.
    OutOfGas,
}

/// `eth_call` at the latest block. An error response that reports an execution outcome is a
/// revert or a gas shortfall; any other error is returned.
pub(super) async fn execute(
    provider: &DynProvider,
    call: TransactionRequest,
) -> Result<Execution, TransportError> {
    let Err(error) = provider.call(call).latest().await else {
        return Ok(Execution::Succeeded);
    };
    let Some(response) = error.as_error_resp() else {
        return Err(error);
    };
    if let Some(data) = response.as_revert_data() {
        return Ok(Execution::Reverted(revert_reason(&data)));
    }
    let message = response.message.to_ascii_lowercase();
    if message.contains("out of gas") || message.contains("intrinsic gas") {
        return Ok(Execution::OutOfGas);
    }
    if message.contains("revert") {
        return Ok(Execution::Reverted(None));
    }
    Err(error)
}

/// A standard `Error(string)` or `Panic(uint256)` reason. Other data isn't shown.
fn revert_reason(data: &[u8]) -> Option<String> {
    let selector = data.get(..4)?;
    if selector != Revert::SELECTOR && selector != Panic::SELECTOR {
        return None;
    }
    let reason = decode_revert_reason(data)?;
    // Contract text is shown to the user: drop control characters and bound its length.
    let reason = reason
        .strip_prefix("revert: ")
        .unwrap_or(&reason)
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect::<String>();
    let reason = reason.trim();
    (!reason.is_empty()).then(|| reason.to_owned())
}

#[cfg(test)]
mod tests {
    use alloy::providers::ProviderBuilder;
    use alloy::rpc::json_rpc::ErrorPayload;
    use alloy::transports::mock::Asserter;

    use super::*;

    fn error_response(message: &str, data: Option<&Bytes>) -> ErrorPayload {
        serde_json::from_str(
            &serde_json::json!({"code": 3, "message": message, "data": data}).to_string(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_failing_pre_hook_reports_its_revert_reason_or_its_gas_limit() {
        let responses = Asserter::new();
        let provider = ProviderBuilder::new()
            .connect_mocked_client(responses.clone())
            .erased();
        let call = TransactionRequest::default();
        let simulate = || simulate_at(&provider, &call, Some(1_100), 1_000);

        // A decodable revert needs no second call.
        responses.push_failure(error_response(
            "execution reverted",
            Some(
                &Revert::from("RelayAdapt: invalid nonce")
                    .abi_encode()
                    .into(),
            ),
        ));
        assert_eq!(
            simulate().await.unwrap(),
            PreHookSimulation::Failed("RelayAdapt: invalid nonce".to_owned())
        );

        // Out of gas under the declared limit, but successful without it.
        responses.push_failure(error_response("out of gas", None));
        responses.push_success(&Bytes::new());
        assert_eq!(
            simulate().await.unwrap(),
            PreHookSimulation::Failed("it needs more gas than its 1000 gas limit".to_owned())
        );

        // Another error response is the endpoint's failure, not the pre-hook's.
        responses.push_failure_msg("upstream unavailable");
        assert!(simulate().await.is_err());

        // Arbitrum makes one uncapped call. A revert fails with its reason.
        let arbitrum = || simulate_at(&provider, &call, None, 1_000);
        responses.push_failure(error_response(
            "execution reverted",
            Some(
                &Revert::from("RelayAdapt: invalid nonce")
                    .abi_encode()
                    .into(),
            ),
        ));
        responses.push_success(&Bytes::new());
        assert_eq!(
            arbitrum().await.unwrap(),
            PreHookSimulation::Failed("RelayAdapt: invalid nonce".to_owned())
        );
        assert_eq!(responses.read_q().len(), 1);
        responses.write_q().clear();

        // Its budget includes L1 posting, so running out of gas is inconclusive, not a failure.
        responses.push_failure(error_response("out of gas", None));
        responses.push_success(&Bytes::new());
        assert_eq!(arbitrum().await.unwrap(), PreHookSimulation::Unavailable);
        assert_eq!(responses.read_q().len(), 1);
    }
}
