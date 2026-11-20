use alloy::network::TransactionBuilder as _;
use alloy::primitives::{Address, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use eyre::{Result, WrapErr, eyre};

use super::contracts::{PublicErc20, PublicWrappedNative};
use super::gas::public_advanced_transaction_payload_fingerprint;
use super::native_shield::{
    NativeShieldPayer, NativeShieldPreparation, ensure_native_shield_delivered,
    stealth_native_shield_contracts,
};
use super::runtime::{
    PublicChainRuntimeConfig, public_shield_token, verified_public_chain_runtime_config,
};
use super::signer::{
    VaultedPublicSigner, admitted_gas_payer_signer, admitted_public_signer_authorized,
};
use super::submission::{
    PublicActionHandoff, PublicActionPools, PublicActionStepEnd, PublicShieldFundingBudget,
    emit_public_action_event, emit_refreshed_public_action_hardware_session, public_action_pools,
    public_action_progress_update, recv_public_action_command, submit_public_action_step_session,
};
use super::types::{
    PublicActionCommandReceiver, PublicActionGasFeeMode, PublicActionGasFeeSelection,
    PublicActionGasLimitStrategy, PublicActionProgressStatus, PublicActionProgressStep,
    PublicActionProgressUpdate, PublicActionSessionEvent, PublicActionSessionEventSender,
    PublicActionStepFeePolicy, PublicAdvancedTransactionAuthorization, PublicAssetId,
    PublicSendRequest, PublicSendResult, PublicShieldApprovalMode, PublicShieldRequest,
    PublicShieldTransactionProfile, PublicTransactionIntent,
};
use crate::block_observer::resolve_source_transaction_by_block;
use crate::desktop::{ActivitySubmission, PublicActivity};
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    ActivityAsset, ActivityCounterparty, ActivityDirection, ActivityKind, ActivityLeg,
    ActivityStep, DesktopViewSession,
};
use crate::{
    HttpContext, OperationHttpClient, RpcRoute, ShieldSendOutput, WalletRpcOrigin,
    report_chain_string,
};

mod action_step;
mod erc20_queries;
mod intent_validation;
mod public_send;
mod public_shield;
mod stealth_shield;

pub(crate) use action_step::*;
pub(crate) use erc20_queries::*;
pub(super) use intent_validation::*;
pub use public_send::*;
pub use public_shield::*;
#[allow(clippy::wildcard_imports)]
use stealth_shield::*;

#[cfg(test)]
mod tests {
    use alloy::primitives::Bytes;

    use super::*;

    #[test]
    fn dynamic_gas_preflight_is_workflow_only() {
        let intent = PublicTransactionIntent::Raw {
            to: Some(Address::ZERO),
            value: U256::ZERO,
            data: Bytes::from(vec![1_u8]),
        };
        let fee = PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
        };
        assert!(
            public_action_authorized_gas_limit(1, Address::ZERO, &intent, None, false, fee)
                .is_err()
        );
        assert_eq!(
            public_action_authorized_gas_limit(1, Address::ZERO, &intent, None, true, fee).unwrap(),
            None
        );
        let transfer = PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Native,
            amount: U256::from(1_u8),
            recipient: Address::ZERO,
        };
        assert!(
            public_action_authorized_gas_limit(1, Address::ZERO, &transfer, None, true, fee)
                .is_err()
        );
    }
}
