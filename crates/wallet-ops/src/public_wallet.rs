mod actions;
mod balance_cache;
mod balances;
mod contracts;
pub(crate) use contracts::PublicErc20;
mod dapp_reads;
mod executor_recovery;
pub(crate) use executor_recovery::submit_executor_recovery_step;
mod gas;
mod recipient;
mod runtime;
pub use recipient::resolve_public_ens_recipient;
mod signer;
mod submission;
#[cfg(test)]
mod tests;
mod transaction_tracker;
mod types;
mod walletconnect;

pub(crate) use actions::submit_public_action_step_with_signer;
pub use actions::{
    submit_public_send, submit_public_send_with_progress, submit_public_shield,
    submit_public_shield_with_progress,
};
pub use balance_cache::{
    PublicBalanceCache, PublicBalanceRefreshCompletion, PublicBalanceRefreshTicket,
    PublicBalanceScope,
};
#[cfg(test)]
pub(crate) use balances::native_asset_for_chain;
pub use balances::{
    public_balance_assets_for_chain, public_balance_refresh_interval_secs, refresh_public_balances,
    refresh_public_balances_at_least,
};
pub use dapp_reads::DappRpcReadClient;
pub(crate) use dapp_reads::stops_dapp_read_retries;
pub(crate) use gas::public_native_action_gas_units_with_buffer;
pub use gas::{
    PUBLIC_NATIVE_UNWRAP_GAS_UNITS, estimate_public_action_gas_cost,
    estimate_public_action_gas_cost_with_profile,
    estimate_public_action_gas_cost_with_profile_and_ceiling, estimate_public_advanced_transaction,
    estimate_public_advanced_transaction_with_fee, estimate_public_native_action_gas_reserve,
    estimate_public_native_action_gas_reserve_with_profile,
    estimate_public_native_action_gas_reserve_with_profile_and_ceiling, project_public_action_fee,
    public_action_maximum_gas_cost_is_significant, public_native_action_gas_reserve,
    public_native_action_gas_units, public_native_action_gas_units_from_walletconnect_intent,
    public_shield_protocol_fee_amount, public_walletconnect_operation_gas_limit,
    quote_public_action_gas_fee, quote_public_action_gas_fee_bundle_with_profile,
    quote_public_action_gas_fee_with_profile, quote_public_action_gas_fee_with_reads,
    resolve_public_action_gas_fee, simulate_public_advanced_transaction_with_fee,
    simulate_public_advanced_transaction_with_fee_and_reads,
};
#[cfg(test)]
use signer::vaulted_public_signer;
pub(crate) use signer::{VaultedPublicSigner, admitted_public_signer};
pub use submission::public_action_replacement_bumped_fee;
pub use submission::{
    sanitize_walletconnect_transaction_request, validate_walletconnect_reviewed_transaction,
    walletconnect_transaction_payload_fingerprint,
};
#[cfg(test)]
pub(crate) use transaction_tracker::test_tracking_context;
pub(crate) use transaction_tracker::{PublicTransactionFamily, PublicTransactionObservationGuard};
pub use transaction_tracker::{
    PublicTransactionLookup, PublicTransactionTracker, PublicTransactionTrackingContext,
};
pub use types::{
    EvmFeeModel, HardwareTrezorPinMatrixProvider, PublicAccountBalance, PublicActionAttemptInfo,
    PublicActionCommand, PublicActionCommandKind, PublicActionCommandReceiver,
    PublicActionCommandSender, PublicActionFeeProjection, PublicActionFeeSource,
    PublicActionGasFeeMode, PublicActionGasFeeQuote, PublicActionGasFeeQuoteBundle,
    PublicActionGasFeeSelection, PublicActionKind, PublicActionProgressStatus,
    PublicActionProgressStep, PublicActionProgressUpdate, PublicActionResolvedGasFee,
    PublicActionSessionEvent, PublicActionSessionEventSender, PublicActionStepFeePolicy,
    PublicAdvancedTransactionAuthorization, PublicAdvancedTransactionEstimate,
    PublicAdvancedTransactionEstimateRequest, PublicAdvancedTransactionSimulationError,
    PublicAssetId, PublicBalanceAmount, PublicBalanceAsset, PublicBalanceEntry,
    PublicBalanceRefreshCoordinator, PublicBalanceSnapshot, PublicSendRequest, PublicSendResult,
    PublicShieldRequest, PublicShieldTransactionProfile, PublicTransactionIntent,
    WalletConnectHardwareTypedDataCapabilityRequest,
    WalletConnectHardwareTypedDataCapabilityResult,
    WalletConnectHardwareTypedDataHashFallbackConfirmationRequired,
    WalletConnectPersonalSignRequest, WalletConnectReviewedFee, WalletConnectReviewedTransaction,
    WalletConnectSendTransactionRequest, WalletConnectSendTransactionResult,
    WalletConnectTypedDataSignRequest,
    is_walletconnect_hardware_typed_data_hash_fallback_confirmation_required,
    walletconnect_hardware_typed_data_hash_fallback_confirmation_session,
};
pub use walletconnect::{
    submit_walletconnect_send_transaction, walletconnect_probe_hardware_typed_data_signing_mode,
    walletconnect_sign_personal_message, walletconnect_sign_typed_data,
    walletconnect_sign_typed_data_v4,
};

#[cfg(test)]
use actions::{
    public_native_shield_transaction_request, public_send_authorized_gas_limit,
    public_send_transaction_request, public_shield_approval_amount,
    public_shield_approval_required,
};
#[cfg(test)]
use balances::{
    plan_public_balance_calls, public_balance_assets_for_chain_with_registry,
    public_balance_snapshot_from_results,
};
#[cfg(test)]
use contracts::PublicRelayAdapt;
#[cfg(test)]
use gas::{
    PUBLIC_ERC20_SEND_GAS_UNITS, PUBLIC_NATIVE_APPROVE_GAS_UNITS,
    PUBLIC_NATIVE_RELAY_ADAPT_SHIELD_GAS_UNITS, PUBLIC_NATIVE_SEND_GAS_UNITS,
    PUBLIC_NATIVE_SHIELD_GAS_UNITS, PUBLIC_NATIVE_WRAP_GAS_UNITS, buffered_advanced_gas_limit,
    public_action_tip_fallback, public_advanced_transaction_payload_fingerprint,
    public_native_action_gas_reserve_with_profile, railway_bnb_gas_fee_quote,
    railway_bnb_gas_fee_quote_bundle, railway_gas_limit, railway_standard_gas_fee_quote,
    railway_standard_gas_fee_quote_bundle,
};
#[cfg(test)]
use runtime::public_chain_runtime_config;
#[cfg(test)]
use signer::{HardwarePublicEvmSigner, verify_hardware_typed_data_signature_address};
#[cfg(test)]
use submission::{
    PublicActionAttemptError, ensure_advanced_gas_estimate_authorized,
    ensure_public_action_broadcast_not_expired, ensure_public_action_command_gas_fee_authorized,
    public_action_before_raw_broadcast_checkpoint, public_action_current_unix_seconds,
    public_action_eip1559_transaction_request, public_action_legacy_transaction_request,
    public_action_native_exposure, public_action_step_initial_gas_fee_selection,
    railway_auto_fee_within_authorized_ceiling,
};
