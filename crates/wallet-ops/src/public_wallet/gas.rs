use std::collections::{HashMap, HashSet};

use alloy::primitives::{B256, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::BlockNumberOrTag;
use alloy::sol_types::{Panic, Revert, SolError, decode_revert_reason};
use eyre::{Result, WrapErr, eyre};
use railgun_wallet::tx::{GasEstimateMode, RailgunGasModel};

use super::actions::{public_send_transaction_request, validate_public_transaction_intent};
use super::runtime::public_chain_runtime_config;
use super::types::{
    PublicActionFeeProjection, PublicActionFeeSource, PublicActionGasFeeQuote,
    PublicActionGasFeeQuoteBundle, PublicActionGasFeeSelection, PublicActionKind,
    PublicActionProgressStep, PublicActionResolvedGasFee, PublicAdvancedTransactionEstimate,
    PublicAdvancedTransactionEstimateRequest, PublicAdvancedTransactionSimulationError,
    PublicAssetId, PublicShieldTransactionProfile, PublicTransactionIntent,
};
use crate::rpc_broker::{EndpointHealthOutcome, RpcCandidates};
use crate::settings::EffectiveChainConfig;
use crate::walletconnect::WalletConnectDecodedCallKind;
use crate::{
    ClassRoute, Eip1559GasCostProjection, GAS_LIMIT_BUFFER, HttpContext, OperationHttpClient,
    RAILGUN_PROTOCOL_FEE_BPS, RpcTrafficClass, SelfBroadcastGasFeeQuote,
    SelfBroadcastResolvedGasFee, SelfBroadcastTipFallback, TRANSACTION_INTRINSIC_GAS,
    expected_eip1559_fee_per_gas, railgun_protocol_fee_amount, resolve_self_broadcast_gas_fee,
};
use railgun_ui::native_usd_micro_value;

mod advanced_estimate;
mod cost_projection;
mod fee_quotes;
mod native_reserve;
mod provider_failures;
mod units;

pub use advanced_estimate::*;
pub use cost_projection::*;
pub use fee_quotes::*;
pub use native_reserve::*;
#[cfg(test)]
use provider_failures::friendly_revert_data;
use provider_failures::{
    classify_broker_provider_failure, classify_provider_failure, select_provider_failure,
    simulation_error_reason, unavailable_simulation_error,
};
pub use units::*;

/// Keeps the tracing target of the moved items at this module's path.
const LOG_TARGET: &str = module_path!();

pub(super) const PUBLIC_NATIVE_SEND_GAS_UNITS: u64 = 21_000;
pub(super) const PUBLIC_ERC20_SEND_GAS_UNITS: u64 = 65_000;
pub(super) const PUBLIC_NATIVE_WRAP_GAS_UNITS: u64 = 50_000;
pub const PUBLIC_NATIVE_UNWRAP_GAS_UNITS: u64 = 40_000;
pub(super) const PUBLIC_NATIVE_APPROVE_GAS_UNITS: u64 = 65_000;
const PUBLIC_ACTION_BNB_CHAIN_ID: u64 = 56;
const RAILWAY_FEE_HISTORY_BLOCKS: u64 = 10;
const RAILWAY_FEE_HISTORY_REWARD_PERCENTILES: [f64; 4] = [40.0, 60.0, 80.0, 95.0];
const RAILWAY_BNB_GAS_PRICE_CAP: u128 = 50_000_000;

#[derive(Debug, Eq, Hash, PartialEq)]
enum ProviderFailureGroup {
    RevertData(Vec<u8>),
    RevertMessage(String),
    JsonRpc { code: i64, message: String },
    Http(u16),
    Other,
}

#[derive(Debug)]
struct ProviderFailure {
    group: ProviderFailureGroup,
    reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undecodable_revert_data_exposes_only_its_selector() {
        assert_eq!(
            friendly_revert_data(&[0x12, 0x34, 0x56, 0x78, 0xab, 0xcd]),
            "Custom error 0x12345678"
        );
    }

    #[test]
    fn broker_simulation_classification_preserves_remote_and_revert_distinction() {
        use crate::{RpcBrokerError, RpcRemoteError, RpcRevert};

        let remote = RpcRemoteError::from(
            serde_json::from_value::<
                alloy::serde::WithOtherFields<
                    alloy::rpc::json_rpc::ErrorPayload<serde_json::Value>,
                >,
            >(serde_json::json!({
                "code": -32000, "message": "execution reverted: provider failure",
            }))
            .unwrap(),
        );
        let error = select_provider_failure(vec![classify_broker_provider_failure(
            &RpcBrokerError::Remote(remote),
        )]);
        assert!(matches!(
            error,
            PublicAdvancedTransactionSimulationError::Unavailable(_)
        ));
        let revert = RpcRevert::from_multicall(Revert::from("Order expired").abi_encode().into());
        let error = select_provider_failure(vec![classify_broker_provider_failure(
            &RpcBrokerError::InnerRevert(revert),
        )]);
        assert!(
            matches!(error, PublicAdvancedTransactionSimulationError::Reverted(reason)
            if reason == "Order expired")
        );
    }

    #[test]
    fn tied_provider_failure_groups_are_unavailable() {
        let error = select_provider_failure(vec![
            ProviderFailure {
                group: ProviderFailureGroup::RevertMessage("expired".to_owned()),
                reason: "Order has expired".to_owned(),
            },
            ProviderFailure {
                group: ProviderFailureGroup::JsonRpc {
                    code: -32000,
                    message: "temporarily unavailable".to_owned(),
                },
                reason: "RPC error -32000: temporarily unavailable".to_owned(),
            },
        ]);
        assert!(matches!(
            error,
            PublicAdvancedTransactionSimulationError::Unavailable(reason)
                if reason == "RPC providers returned conflicting simulation results."
        ));
    }
}
