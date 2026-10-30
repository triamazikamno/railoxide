use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::{TransactionRequest, transaction::AccessList};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::RpcRead;
use crate::hardware::HardwareTypedDataSigningMode;
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    DesktopVaultStore, DesktopViewSession, HardwareProfileSession, PublicAccountMetadata,
};
use crate::walletconnect::WalletConnectDecodedTransaction;

pub type PublicActionGasFeeQuote = crate::SelfBroadcastGasFeeQuote;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TxReceiptOutput {
    pub tx_hash: String,
    pub status: bool,
    pub block_number: u64,
    pub gas_used: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_address: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ShieldSendOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrap: Option<TxReceiptOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approve: Option<TxReceiptOutput>,
    pub shield: TxReceiptOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvmFeeModel {
    Legacy,
    Eip1559,
}
pub type PublicActionGasFeeSelection = crate::SelfBroadcastGasFeeSelection;
pub type PublicActionResolvedGasFee = crate::SelfBroadcastResolvedGasFee;

#[derive(Debug, Error)]
pub enum PublicAdvancedTransactionSimulationError {
    #[error(transparent)]
    RpcRead(crate::rpc_broker::RpcBrokerError),
    #[error("simulation would revert: {0}")]
    Reverted(String),
    #[error("simulation unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicActionFeeSource {
    OperationTable,
    NetworkSimulation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicActionFeeProjection {
    pub source: PublicActionFeeSource,
    pub raw_gas_limit: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub expected_fee_per_gas: u128,
    pub expected_gas_cost: U256,
    pub maximum_gas_cost: U256,
    pub expected_native_usd_micro_value: Option<U256>,
    pub maximum_native_usd_micro_value: Option<U256>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicActionGasFeeQuoteBundle {
    pub standard: PublicActionGasFeeQuote,
    pub authorization_ceiling: PublicActionGasFeeSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicActionGasFeeMode {
    Auto,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicActionStepFeePolicy {
    Captured,
    RefreshRailwayStandard,
    Custom,
}
pub type PublicActionCommandKind = crate::SelfBroadcastCommandKind;
pub type PublicActionCommand = crate::SelfBroadcastCommand;
pub type PublicActionCommandSender = tokio::sync::mpsc::UnboundedSender<PublicActionCommand>;
pub type PublicActionCommandReceiver = tokio::sync::mpsc::UnboundedReceiver<PublicActionCommand>;
pub type PublicActionAttemptInfo = crate::SelfBroadcastAttemptInfo;
pub type PublicActionSessionEventSender =
    tokio::sync::mpsc::UnboundedSender<PublicActionSessionEvent>;
#[cfg(feature = "hardware")]
pub type HardwareTrezorPinMatrixProvider = crate::hardware::trezor::TrezorPinMatrixProvider;
#[cfg(not(feature = "hardware"))]
pub type HardwareTrezorPinMatrixProvider = ();

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PublicAssetId {
    Native,
    Erc20(Address),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicShieldTransactionProfile {
    Railoxide,
    Railway,
}

/// How a Public Shield approves its ERC-20, a fallback native shield's wrapped token
/// included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicShieldApprovalMode {
    /// Approves exactly the shield amount before every shield.
    Exact,
    /// Approves the maximum amount, only when the allowance is below the shield amount.
    UnlimitedWhenShort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublicActionGasLimitStrategy {
    ChainBuffer,
    RailwayEstimate120,
    /// The profile's buffer over the final raw estimate of a prepared native shield, signed at
    /// most at its reviewed limit and refused only when the raw estimate itself exceeds that
    /// limit.
    NativeShieldPrepared {
        reviewed_gas_limit: u64,
        profile: PublicShieldTransactionProfile,
    },
}

impl PublicShieldTransactionProfile {
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Railoxide => "Railoxide",
            Self::Railway => "Mimic Railway",
        }
    }

    /// Railway prices only BNB ERC-20 actions with a legacy envelope; its native shield is type 4.
    #[must_use]
    pub const fn uses_legacy_envelope(self, chain_id: u64, asset: PublicAssetId) -> bool {
        matches!(self, Self::Railway) && chain_id == 56 && matches!(asset, PublicAssetId::Erc20(_))
    }

    /// A prepared native shield instead signs within its reviewed limit.
    pub(super) const fn gas_limit_strategy(self) -> PublicActionGasLimitStrategy {
        match self {
            Self::Railway => PublicActionGasLimitStrategy::RailwayEstimate120,
            Self::Railoxide => PublicActionGasLimitStrategy::ChainBuffer,
        }
    }

    /// The approval mode a shield signs with: Mimic Railway locks Railway's unlimited approval,
    /// Railoxide keeps the `chosen` one.
    #[must_use]
    pub const fn approval_mode(self, chosen: PublicShieldApprovalMode) -> PublicShieldApprovalMode {
        match self {
            Self::Railway => PublicShieldApprovalMode::UnlimitedWhenShort,
            Self::Railoxide => chosen,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicTransactionIntent {
    Transfer {
        asset: PublicAssetId,
        amount: U256,
        recipient: Address,
    },
    Raw {
        to: Option<Address>,
        value: U256,
        data: Bytes,
    },
    WrapNative {
        wrapped: Address,
        amount: U256,
    },
    UnwrapNative {
        wrapped: Address,
        amount: U256,
    },
}

impl PublicAssetId {
    #[must_use]
    pub const fn token_address(self) -> Option<Address> {
        match self {
            Self::Native => None,
            Self::Erc20(token) => Some(token),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicActionKind {
    Shield,
    Send,
    Wrap,
    Unwrap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicBalanceAsset {
    pub id: PublicAssetId,
    pub symbol: String,
    pub decimals: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicBalanceAmount {
    Available(U256),
    Unavailable,
}

impl PublicBalanceAmount {
    #[must_use]
    pub const fn amount(&self) -> Option<U256> {
        match self {
            Self::Available(amount) => Some(*amount),
            Self::Unavailable => None,
        }
    }

    #[must_use]
    pub fn is_zero(&self) -> bool {
        matches!(self, Self::Available(amount) if amount.is_zero())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicBalanceEntry {
    pub asset: PublicBalanceAsset,
    pub amount: PublicBalanceAmount,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicAccountBalance {
    pub account: PublicAccountMetadata,
    pub balances: Vec<PublicBalanceEntry>,
    /// Monotonic start of a complete account observation; never persisted.
    pub observed_at: Option<Instant>,
    /// Block number the account's read reported it was answered at.
    pub observed_block: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicBalanceSnapshot {
    pub chain_id: u64,
    /// UI refresh time; carried accounts retain their own observation freshness.
    pub refreshed_at: SystemTime,
    pub accounts: Vec<PublicAccountBalance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedPublicBalanceCall {
    pub(crate) public_account_uuid: String,
    pub(crate) asset: PublicBalanceAsset,
    pub(crate) read: RpcRead,
}

#[derive(Default)]
pub struct PublicBalanceRefreshCoordinator {
    refreshing: Arc<AtomicBool>,
}

pub struct PublicBalanceRefreshGuard {
    refreshing: Arc<AtomicBool>,
}

impl PublicBalanceRefreshCoordinator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn try_begin(&self) -> Option<PublicBalanceRefreshGuard> {
        self.refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| PublicBalanceRefreshGuard {
                refreshing: Arc::clone(&self.refreshing),
            })
    }

    #[must_use]
    pub fn is_refreshing(&self) -> bool {
        self.refreshing.load(Ordering::Acquire)
    }
}

impl Drop for PublicBalanceRefreshGuard {
    fn drop(&mut self) {
        self.refreshing.store(false, Ordering::Release);
    }
}

pub struct PublicSendRequest {
    pub executor_owner: Option<Arc<crate::ExecutorOwner>>,
    pub transaction_tracking: Option<crate::PublicTransactionTrackingContext>,
    pub chain_id: u64,
    pub effective_chain: EffectiveChainConfig,
    pub view_session: Arc<DesktopViewSession>,
    pub vault_store: Arc<DesktopVaultStore>,
    pub authorization: Option<crate::DesktopPrivateSpendAuthorization>,
    pub trezor_app_passphrase: Option<Zeroizing<String>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
    pub intent: PublicTransactionIntent,
    pub advanced_authorization: Option<PublicAdvancedTransactionAuthorization>,
    pub gas_fee: PublicActionGasFeeSelection,
    pub command_rx: Option<PublicActionCommandReceiver>,
    pub event_tx: Option<PublicActionSessionEventSender>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicAdvancedTransactionAuthorization {
    pub payload_fingerprint: B256,
    pub gas_limit: u64,
}

pub struct PublicAdvancedTransactionEstimateRequest {
    pub chain_id: u64,
    pub effective_chain: EffectiveChainConfig,
    pub from: Address,
    pub intent: PublicTransactionIntent,
    pub gas_fee: PublicActionGasFeeSelection,
    pub access_list: Option<AccessList>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicAdvancedTransactionEstimate {
    pub payload_fingerprint: B256,
    pub raw_gas_limit: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub expected_fee_per_gas: u128,
    pub expected_gas_cost: U256,
    pub max_gas_cost: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicSendResult {
    pub tx: TxReceiptOutput,
}

pub struct PublicShieldRequest {
    pub executor_owner: Option<Arc<crate::ExecutorOwner>>,
    pub transaction_tracking: Option<crate::PublicTransactionTrackingContext>,
    pub chain_id: u64,
    pub effective_chain: EffectiveChainConfig,
    pub view_session: Arc<DesktopViewSession>,
    pub vault_store: Arc<DesktopVaultStore>,
    pub authorization: Option<crate::DesktopPrivateSpendAuthorization>,
    pub trezor_app_passphrase: Option<Zeroizing<String>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
    pub asset: PublicAssetId,
    pub amount: U256,
    pub profile: PublicShieldTransactionProfile,
    /// Chosen for Railoxide; Mimic Railway signs its own policy.
    pub approval_mode: PublicShieldApprovalMode,
    /// Required by, and only by, a stealth-account native shield. A native shield without one
    /// takes the Railoxide wrap fallback.
    pub native_preparation: Option<Arc<super::NativeShieldPreparation>>,
    /// The stealth account operation a native shield is sent from. A retry of the same
    /// shield passes the same one.
    pub native_operation: Option<crate::vault::ExecutorOperationId>,
    pub gas_fee: PublicActionGasFeeSelection,
    pub gas_fee_mode: PublicActionGasFeeMode,
    pub authorized_fee_ceiling: PublicActionGasFeeSelection,
    pub command_rx: Option<PublicActionCommandReceiver>,
    pub event_tx: Option<PublicActionSessionEventSender>,
}

pub struct WalletConnectPersonalSignRequest {
    pub chain_id: u64,
    pub executor_owner: Option<Arc<crate::ExecutorOwner>>,
    pub request_control: Option<crate::dapp_request::DappRequestControl>,
    pub view_session: Arc<DesktopViewSession>,
    pub vault_store: Arc<DesktopVaultStore>,
    pub authorization: Option<crate::DesktopPrivateSpendAuthorization>,
    pub trezor_app_passphrase: Option<Zeroizing<String>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
    pub message: Vec<u8>,
    pub event_tx: Option<PublicActionSessionEventSender>,
}

pub struct WalletConnectTypedDataSignRequest {
    pub chain_id: u64,
    pub executor_owner: Option<Arc<crate::ExecutorOwner>>,
    pub request_control: Option<crate::dapp_request::DappRequestControl>,
    pub view_session: Arc<DesktopViewSession>,
    pub vault_store: Arc<DesktopVaultStore>,
    pub authorization: Option<crate::DesktopPrivateSpendAuthorization>,
    pub trezor_app_passphrase: Option<Zeroizing<String>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
    pub typed_data: Value,
    pub hash_fallback_confirmed: bool,
    pub event_tx: Option<PublicActionSessionEventSender>,
}

pub struct WalletConnectHardwareTypedDataHashFallbackConfirmationRequired {
    refreshed_hardware_session: Option<HardwareProfileSession>,
}

impl WalletConnectHardwareTypedDataHashFallbackConfirmationRequired {
    #[must_use]
    pub const fn new(refreshed_hardware_session: Option<HardwareProfileSession>) -> Self {
        Self {
            refreshed_hardware_session,
        }
    }

    #[must_use]
    pub fn refreshed_hardware_session(&self) -> Option<HardwareProfileSession> {
        self.refreshed_hardware_session.clone()
    }
}

impl fmt::Debug for WalletConnectHardwareTypedDataHashFallbackConfirmationRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WalletConnectHardwareTypedDataHashFallbackConfirmationRequired")
    }
}

impl fmt::Display for WalletConnectHardwareTypedDataHashFallbackConfirmationRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "WalletConnect hardware typed-data hash fallback requires confirmation before device approval",
        )
    }
}

impl std::error::Error for WalletConnectHardwareTypedDataHashFallbackConfirmationRequired {}

#[must_use]
pub fn walletconnect_hardware_typed_data_hash_fallback_confirmation_session(
    error: &eyre::Report,
) -> Option<HardwareProfileSession> {
    error
        .downcast_ref::<WalletConnectHardwareTypedDataHashFallbackConfirmationRequired>()
        .and_then(WalletConnectHardwareTypedDataHashFallbackConfirmationRequired::refreshed_hardware_session)
}

#[must_use]
pub fn is_walletconnect_hardware_typed_data_hash_fallback_confirmation_required(
    error: &eyre::Report,
) -> bool {
    error
        .downcast_ref::<WalletConnectHardwareTypedDataHashFallbackConfirmationRequired>()
        .is_some()
}

pub struct WalletConnectHardwareTypedDataCapabilityRequest {
    pub view_session: Arc<DesktopViewSession>,
    pub vault_store: Arc<DesktopVaultStore>,
    pub trezor_app_passphrase: Option<Zeroizing<String>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletConnectHardwareTypedDataCapabilityResult {
    pub mode: HardwareTypedDataSigningMode,
    pub refreshed_hardware_session: Option<HardwareProfileSession>,
}

pub struct WalletConnectSendTransactionRequest {
    pub executor_owner: Option<Arc<crate::ExecutorOwner>>,
    pub request_control: Option<crate::dapp_request::DappRequestControl>,
    pub rpc_reads: Option<super::DappRpcReadClient>,
    pub transaction_tracking: Option<crate::PublicTransactionTrackingContext>,
    pub chain_id: u64,
    pub effective_chain: EffectiveChainConfig,
    pub view_session: Arc<DesktopViewSession>,
    pub vault_store: Arc<DesktopVaultStore>,
    pub authorization: Option<crate::DesktopPrivateSpendAuthorization>,
    pub trezor_app_passphrase: Option<Zeroizing<String>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
    pub tx_req: TransactionRequest,
    pub decoded_transaction: Option<WalletConnectDecodedTransaction>,
    pub reviewed_transaction: Option<WalletConnectReviewedTransaction>,
    pub reviewed_fee: Option<WalletConnectReviewedFee>,
    pub gas_fee: PublicActionGasFeeSelection,
    pub expiry_timestamp: Option<u64>,
    pub event_tx: Option<PublicActionSessionEventSender>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalletConnectReviewedFee {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalletConnectReviewedTransaction {
    pub payload_fingerprint: B256,
    pub gas_limit: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletConnectSendTransactionResult {
    pub tx_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PublicActionProgressStep {
    ShieldKey,
    Send,
    Wrap,
    Unwrap,
    Approve,
    Shield,
    Sponsor,
    Unsponsor,
    CallVote,
    Vote,
    GovernanceApprove,
    Stake,
    Delegate,
    Undelegate,
    Unlock,
    PrincipalClaim,
    RewardClaim(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicActionProgressStatus {
    Pending,
    Done,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicActionProgressUpdate {
    pub step: PublicActionProgressStep,
    pub status: PublicActionProgressStatus,
    pub tx_hash: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicActionSessionEvent {
    StepFailed {
        step: PublicActionProgressStep,
        message: String,
    },
    AttemptHandoff {
        step: PublicActionProgressStep,
    },
    AttemptSubmitted {
        step: PublicActionProgressStep,
        attempt: PublicActionAttemptInfo,
    },
    AttemptRejected {
        step: PublicActionProgressStep,
        message: String,
    },
    FeeAuthorizationRequired {
        step: PublicActionProgressStep,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        message: String,
    },
    HardwareApprovalStarted,
    HardwareApprovalCompleted,
    HardwareApprovalFailed {
        message: String,
    },
    HardwareProfileSessionRefreshed {
        session: HardwareProfileSession,
    },
}
