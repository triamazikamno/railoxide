use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::{TransactionRequest, transaction::AccessList};
use serde_json::Value;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::TxReceiptOutput;
use crate::hardware::HardwareTypedDataSigningMode;
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    DesktopVaultStore, DesktopViewSession, HardwareProfileSession, ProtectedSoftwareSeedSession,
    PublicAccountMetadata,
};
use crate::walletconnect::WalletConnectDecodedTransaction;

pub type PublicActionGasFeeQuote = crate::SelfBroadcastGasFeeQuote;
pub type PublicActionGasFeeSelection = crate::SelfBroadcastGasFeeSelection;
pub type PublicActionResolvedGasFee = crate::SelfBroadcastResolvedGasFee;

#[derive(Debug, Error)]
pub enum PublicAdvancedTransactionSimulationError {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublicActionGasLimitStrategy {
    ChainBuffer,
    RailwayEstimate120,
    RailwayNativeFixed,
}

impl PublicShieldTransactionProfile {
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Railoxide => "Railoxide",
            Self::Railway => "Mimic Railway",
        }
    }

    #[must_use]
    pub const fn uses_legacy_envelope(self, chain_id: u64) -> bool {
        matches!(self, Self::Railway) && chain_id == 56
    }

    pub(super) const fn gas_limit_strategy(
        self,
        asset: PublicAssetId,
    ) -> PublicActionGasLimitStrategy {
        match (self, asset) {
            (Self::Railway, PublicAssetId::Native) => {
                PublicActionGasLimitStrategy::RailwayNativeFixed
            }
            (Self::Railway, PublicAssetId::Erc20(_)) => {
                PublicActionGasLimitStrategy::RailwayEstimate120
            }
            (Self::Railoxide, _) => PublicActionGasLimitStrategy::ChainBuffer,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicBalanceSnapshot {
    pub chain_id: u64,
    pub refreshed_at: SystemTime,
    pub accounts: Vec<PublicAccountBalance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedPublicBalanceCall {
    pub(crate) public_account_uuid: String,
    pub(crate) account: Address,
    pub(crate) asset: PublicBalanceAsset,
    pub(crate) target: Address,
    pub(crate) data: Vec<u8>,
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
    pub chain_id: u64,
    pub effective_chain: Option<EffectiveChainConfig>,
    pub view_session: Arc<DesktopViewSession>,
    pub vault_store: Arc<DesktopVaultStore>,
    pub vault_password: Zeroizing<String>,
    pub protected_software_seed_session: Option<Arc<ProtectedSoftwareSeedSession>>,
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
    pub effective_chain: Option<EffectiveChainConfig>,
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
    pub chain_id: u64,
    pub effective_chain: Option<EffectiveChainConfig>,
    pub view_session: Arc<DesktopViewSession>,
    pub vault_store: Arc<DesktopVaultStore>,
    pub vault_password: Zeroizing<String>,
    pub protected_software_seed_session: Option<Arc<ProtectedSoftwareSeedSession>>,
    pub trezor_app_passphrase: Option<Zeroizing<String>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
    pub asset: PublicAssetId,
    pub amount: U256,
    pub profile: PublicShieldTransactionProfile,
    pub gas_fee: PublicActionGasFeeSelection,
    pub gas_fee_mode: PublicActionGasFeeMode,
    pub authorized_fee_ceiling: PublicActionGasFeeSelection,
    pub command_rx: Option<PublicActionCommandReceiver>,
    pub event_tx: Option<PublicActionSessionEventSender>,
}

pub struct WalletConnectPersonalSignRequest {
    pub view_session: Arc<DesktopViewSession>,
    pub vault_store: Arc<DesktopVaultStore>,
    pub vault_password: Zeroizing<String>,
    pub protected_software_seed_session: Option<Arc<ProtectedSoftwareSeedSession>>,
    pub trezor_app_passphrase: Option<Zeroizing<String>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
    pub message: Vec<u8>,
    pub event_tx: Option<PublicActionSessionEventSender>,
}

pub struct WalletConnectTypedDataSignRequest {
    pub view_session: Arc<DesktopViewSession>,
    pub vault_store: Arc<DesktopVaultStore>,
    pub vault_password: Zeroizing<String>,
    pub protected_software_seed_session: Option<Arc<ProtectedSoftwareSeedSession>>,
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
    pub chain_id: u64,
    pub effective_chain: Option<EffectiveChainConfig>,
    pub view_session: Arc<DesktopViewSession>,
    pub vault_store: Arc<DesktopVaultStore>,
    pub vault_password: Zeroizing<String>,
    pub protected_software_seed_session: Option<Arc<ProtectedSoftwareSeedSession>>,
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
