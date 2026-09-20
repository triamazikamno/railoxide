use super::*;
use alloy::uint;
use railgun_wallet::RailgunSpendSigner;

pub(crate) use crate::poi_contexts::{
    active_list_pre_transaction_pois, persist_pending_composite_unshield_output_poi_contexts,
    persist_pending_mixed_output_poi_contexts, persist_pending_send_output_poi_contexts,
    persist_pending_unshield_output_poi_contexts, public_broadcaster_pre_transaction_pois,
};
pub(crate) use crate::utxos::utxo_outputs_from_utxos;

#[cfg(test)]
pub(crate) use crate::poi_contexts::{
    build_pending_output_poi_context_records, pending_send_output_role_plans,
    pending_unshield_output_role_plans,
};

pub(crate) const DEFAULT_QUERY_RPC_COOLDOWN: Duration = Duration::from_secs(5);
pub(crate) const DEFAULT_BLOCK_RANGE: u64 = 500;
pub(crate) const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(15);
pub(crate) const GAS_LIMIT_BUFFER: u64 = 100_000;
pub(crate) const GAS_PRICE_BUFFER_NUMERATOR: u128 = 105;
pub(crate) const GAS_PRICE_BUFFER_DENOMINATOR: u128 = 100;
pub(crate) const PUBLIC_BROADCASTER_FEE_ATTEMPTS: usize = 5;
pub(crate) const PUBLIC_BROADCASTER_REPUBLISH_INTERVAL: Duration = Duration::from_secs(5);
pub(crate) const PUBLIC_BROADCASTER_FEE_BUFFER_DIVISOR: U256 = uint!(100_U256);
pub(crate) const APPROX_BASE_GAS: u64 = 650_000;
pub(crate) const APPROX_GAS_PER_INPUT: u64 = 155_000;
pub(crate) const APPROX_GAS_PER_PRIVATE_OUTPUT: u64 = 85_000;
pub(crate) const APPROX_GAS_PER_PUBLIC_OUTPUT: u64 = 65_000;
pub(crate) const APPROX_GAS_PER_TRANSACTION: u64 = 120_000;
pub(crate) const APPROX_SEND_EXTRA_GAS: u64 = 40_000;
pub(crate) const APPROX_UNWRAP_EXTRA_GAS: u64 = 50_000;
pub(crate) const APPROX_SAFETY_GAS: u64 = 150_000;
pub(crate) const APPROX_GAS_UPLIFT_NUMERATOR: u64 = 112;
pub(crate) const APPROX_GAS_UPLIFT_DENOMINATOR: u64 = 100;
pub(crate) const PUBLIC_BROADCASTER_MAX_ENTERED_AMOUNT_ERROR: &str =
    "public broadcaster max entered amount: ";
pub(crate) const PUBLIC_BROADCASTER_FEE_TOKEN_MAX_SPENDABLE_ERROR: &str =
    "public broadcaster fee-token max spendable: ";
pub(crate) const PUBLIC_BROADCASTER_REQUIRED_FEE_ERROR: &str = "; required fee: ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopWalletSyncStartPolicy {
    ImportedHistoricalBackfill,
    CurrentSafeHeadNoBackfill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatedWalletChainInitPolicy {
    InitialCreate,
    Resumed,
}

impl CreatedWalletChainInitPolicy {
    #[must_use]
    pub const fn sync_start_policy(self) -> DesktopWalletSyncStartPolicy {
        match self {
            Self::InitialCreate => DesktopWalletSyncStartPolicy::CurrentSafeHeadNoBackfill,
            Self::Resumed => DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
        }
    }
}

impl From<vault::WalletSource> for DesktopWalletSyncStartPolicy {
    fn from(_value: vault::WalletSource) -> Self {
        Self::ImportedHistoricalBackfill
    }
}

impl From<&vault::WalletMetadataBundle> for DesktopWalletSyncStartPolicy {
    fn from(_value: &vault::WalletMetadataBundle) -> Self {
        Self::ImportedHistoricalBackfill
    }
}

pub enum DesktopPrivateSpendAuthorization {
    VaultPassword(Zeroizing<String>),
    ProtectedSoftwareSeed {
        password: Zeroizing<String>,
        session: Arc<vault::ProtectedSoftwareSeedSession>,
    },
    PreauthorizedSigner(vault::SoftwareRailgunSpendSigner),
}

pub(crate) enum DesktopPrivateSpendSigner<'a> {
    Owned(Box<vault::SoftwareRailgunSpendSigner>),
    Borrowed(&'a vault::SoftwareRailgunSpendSigner),
}

impl RailgunSpendSigner for DesktopPrivateSpendSigner<'_> {
    fn spending_public_key(&self) -> [U256; 2] {
        match self {
            Self::Owned(signer) => signer.spending_public_key(),
            Self::Borrowed(signer) => signer.spending_public_key(),
        }
    }

    fn sign_spend_message(&self, msg: U256) -> [U256; 3] {
        match self {
            Self::Owned(signer) => signer.sign_spend_message(msg),
            Self::Borrowed(signer) => signer.sign_spend_message(msg),
        }
    }
}

impl DesktopPrivateSpendAuthorization {
    pub(crate) fn executor_spend_grant<'a>(
        &'a self,
        store: &vault::DesktopVaultStore,
    ) -> Result<(
        vault::SpendGrant,
        Option<&'a vault::ProtectedSoftwareSeedSession>,
    )> {
        let (password, seed) = match self {
            Self::VaultPassword(password) => (password, None),
            Self::ProtectedSoftwareSeed { password, session } => (password, Some(session.as_ref())),
            Self::PreauthorizedSigner(_) => {
                return Err(eyre!(
                    "executor signing requires authorization for the original software seed"
                ));
            }
        };
        Ok((store.create_spend_grant(password.as_str())?, seed))
    }

    pub(crate) fn signer<'a>(
        &'a self,
        vault_store: &vault::DesktopVaultStore,
        view_session: &vault::DesktopViewSession,
        operation: &'static str,
    ) -> Result<DesktopPrivateSpendSigner<'a>> {
        match self {
            Self::VaultPassword(password) => {
                let mut grant = vault_store
                    .create_spend_grant(password.as_str())
                    .wrap_err_with(|| format!("authorize {operation} spend"))?;
                let signer = vault_store
                    .railgun_spend_signer_for_session(&mut grant, view_session, None)
                    .wrap_err_with(|| format!("load {operation} spend signer"))?;
                Ok(DesktopPrivateSpendSigner::Owned(Box::new(signer)))
            }
            Self::ProtectedSoftwareSeed { password, session } => {
                let mut grant = vault_store
                    .create_spend_grant(password.as_str())
                    .wrap_err_with(|| format!("authorize {operation} spend"))?;
                let signer = vault_store
                    .railgun_spend_signer_for_session(
                        &mut grant,
                        view_session,
                        Some(session.as_ref()),
                    )
                    .wrap_err_with(|| format!("load {operation} spend signer"))?;
                Ok(DesktopPrivateSpendSigner::Owned(Box::new(signer)))
            }
            Self::PreauthorizedSigner(signer) => Ok(DesktopPrivateSpendSigner::Borrowed(signer)),
        }
    }

    pub(crate) fn into_signer(
        self,
        vault_store: &vault::DesktopVaultStore,
        view_session: &vault::DesktopViewSession,
        operation: &'static str,
    ) -> Result<vault::SoftwareRailgunSpendSigner> {
        match self {
            Self::VaultPassword(password) => {
                let mut grant = vault_store
                    .create_spend_grant(password.as_str())
                    .wrap_err_with(|| format!("authorize {operation} spend"))?;
                vault_store
                    .railgun_spend_signer_for_session(&mut grant, view_session, None)
                    .wrap_err_with(|| format!("load {operation} spend signer"))
            }
            Self::ProtectedSoftwareSeed { password, session } => {
                let mut grant = vault_store
                    .create_spend_grant(password.as_str())
                    .wrap_err_with(|| format!("authorize {operation} spend"))?;
                vault_store
                    .railgun_spend_signer_for_session(
                        &mut grant,
                        view_session,
                        Some(session.as_ref()),
                    )
                    .wrap_err_with(|| format!("load {operation} spend signer"))
            }
            Self::PreauthorizedSigner(signer) => Ok(signer),
        }
    }

    pub fn public_signing_parts(
        self,
    ) -> Result<(
        Zeroizing<String>,
        Option<Arc<vault::ProtectedSoftwareSeedSession>>,
    )> {
        match self {
            Self::VaultPassword(password) => Ok((password, None)),
            Self::ProtectedSoftwareSeed { password, session } => Ok((password, Some(session))),
            Self::PreauthorizedSigner(_) => Err(eyre!(
                "preauthorized Railgun signer cannot authorize a Public action"
            )),
        }
    }

    #[must_use]
    pub fn protected_seed_session(&self) -> Option<Arc<vault::ProtectedSoftwareSeedSession>> {
        match self {
            Self::ProtectedSoftwareSeed { session, .. } => Some(Arc::clone(session)),
            Self::VaultPassword(_) | Self::PreauthorizedSigner(_) => None,
        }
    }
}

pub struct ViewWalletChainSessionRequest {
    pub view_session: Arc<vault::DesktopViewSession>,
    pub wallet_scope_generation: u64,
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub sync_start_policy: DesktopWalletSyncStartPolicy,
    pub init_block_number: Option<u64>,
    pub sync_to_block: Option<u64>,
    pub use_indexed_wallet_catch_up: bool,
    pub poi_read_source: PoiReadSource,
    pub rewind_wallet_cache: bool,
    pub progress_tx: Option<SyncProgressSender>,
}

pub struct DesktopUnshieldCalldataRequest {
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub spend_authorization: DesktopPrivateSpendAuthorization,
    pub token: Address,
    pub fee_token: Address,
    pub amount: U256,
    pub fee_mode: FeeHandlingMode,
    pub recipient: Address,
    pub unwrap: bool,
    pub native_top_up: Option<DesktopNativeTopUpRequest>,
    pub verify_proof: bool,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
}

pub struct DesktopSendCalldataRequest {
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub spend_authorization: DesktopPrivateSpendAuthorization,
    pub token: Address,
    pub fee_token: Address,
    pub amount: U256,
    pub recipient: String,
    pub verify_proof: bool,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
}

pub const SELF_BROADCAST_AUTO_MAX_FEE_NUMERATOR: u128 = 120;
pub const SELF_BROADCAST_AUTO_MAX_FEE_DENOMINATOR: u128 = 100;
pub const SELF_BROADCAST_MIN_PRIORITY_FEE_PER_GAS: u128 = 1;
pub const SELF_BROADCAST_REPLACEMENT_BUMP_NUMERATOR: u128 = 9;
pub const SELF_BROADCAST_REPLACEMENT_BUMP_DENOMINATOR: u128 = 8;
pub const EIP1559_EXPECTED_BASE_FEE_DENOMINATOR: u128 = 8;
pub(crate) const SELF_BROADCAST_FEE_HISTORY_BLOCKS: u64 = 5;
pub(crate) const SELF_BROADCAST_FEE_HISTORY_REWARD_PERCENTILES: [f64; 3] = [25.0, 50.0, 75.0];
pub(crate) const SELF_BROADCAST_DIRECT_FEE_QUOTE_GRACE: Duration = Duration::from_millis(750);
pub(crate) const SELF_BROADCAST_DIRECT_FEE_QUOTE_DEADLINE: Duration = Duration::from_secs(8);
pub(crate) const SELF_BROADCAST_TOR_FEE_QUOTE_GRACE: Duration = Duration::from_secs(2);
pub(crate) const SELF_BROADCAST_TOR_FEE_QUOTE_DEADLINE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelfBroadcastTipFallback {
    Minimum,
    RpcGasPrice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfBroadcastGasFeeQuote {
    /// Set only after a successful public-chain header observation. None is not legacy evidence.
    pub observed_fee_model: Option<crate::EvmFeeModel>,
    pub rpc_gas_price: u128,
    pub current_base_fee_per_gas: Option<u128>,
    pub suggested_max_fee_per_gas: u128,
    pub suggested_max_priority_fee_per_gas: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eip1559GasCostProjection {
    pub expected_fee_per_gas: u128,
    pub maximum_fee_per_gas: u128,
    pub expected_cost: U256,
    pub maximum_cost: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopSelfBroadcastProtocolFee {
    pub token: Address,
    pub amount: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopSelfBroadcastCostEstimate {
    pub gas_limit: u64,
    pub gas_cost: Eip1559GasCostProjection,
    pub protocol_fees: Vec<DesktopSelfBroadcastProtocolFee>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelfBroadcastGasFeeSelection {
    #[default]
    Auto,
    Custom {
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfBroadcastCommandKind {
    Retry,
    Replacement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfBroadcastCommand {
    pub kind: SelfBroadcastCommandKind,
    pub gas_fee: SelfBroadcastGasFeeSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfBroadcastAttemptInfo {
    pub tx_hash: String,
    pub nonce: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfBroadcastSessionEvent {
    PendingOutputPoiProofsRequired {
        required: bool,
    },
    StepFailed {
        stage: TransactionGenerationStage,
        message: String,
    },
    AttemptSubmitted(SelfBroadcastAttemptInfo),
    AttemptRejected {
        stage: TransactionGenerationStage,
        message: String,
    },
    HardwareProfileSessionRefreshed {
        session: vault::HardwareProfileSession,
    },
}

pub type SelfBroadcastCommandSender = mpsc::UnboundedSender<SelfBroadcastCommand>;
pub type SelfBroadcastCommandReceiver = mpsc::UnboundedReceiver<SelfBroadcastCommand>;
pub type SelfBroadcastSessionEventSender = mpsc::UnboundedSender<SelfBroadcastSessionEvent>;

impl SelfBroadcastGasFeeQuote {
    #[must_use]
    pub const fn from_rpc_gas_price(rpc_gas_price: u128) -> Self {
        let suggested_max_fee_per_gas = self_broadcast_auto_max_fee_per_gas(rpc_gas_price);
        let suggested_max_fee_per_gas = if suggested_max_fee_per_gas > 0 {
            suggested_max_fee_per_gas
        } else {
            SELF_BROADCAST_MIN_PRIORITY_FEE_PER_GAS
        };
        Self {
            rpc_gas_price,
            observed_fee_model: None,
            current_base_fee_per_gas: None,
            suggested_max_fee_per_gas,
            suggested_max_priority_fee_per_gas: SELF_BROADCAST_MIN_PRIORITY_FEE_PER_GAS,
        }
    }
}

#[must_use]
pub fn expected_eip1559_fee_per_gas(
    quote: SelfBroadcastGasFeeQuote,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> u128 {
    let expected = quote
        .current_base_fee_per_gas
        .map_or(max_fee_per_gas, |base_fee| {
            let cushion = base_fee / EIP1559_EXPECTED_BASE_FEE_DENOMINATOR
                + u128::from(base_fee % EIP1559_EXPECTED_BASE_FEE_DENOMINATOR != 0);
            base_fee
                .saturating_add(cushion)
                .saturating_add(max_priority_fee_per_gas)
        });
    expected.min(max_fee_per_gas)
}

#[must_use]
pub fn eip1559_gas_cost_projection(
    gas_limit: u64,
    quote: SelfBroadcastGasFeeQuote,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> Eip1559GasCostProjection {
    let expected_fee_per_gas =
        expected_eip1559_fee_per_gas(quote, max_fee_per_gas, max_priority_fee_per_gas);
    Eip1559GasCostProjection {
        expected_fee_per_gas,
        maximum_fee_per_gas: max_fee_per_gas,
        expected_cost: U256::from(gas_limit) * U256::from(expected_fee_per_gas),
        maximum_cost: U256::from(gas_limit) * U256::from(max_fee_per_gas),
    }
}

#[must_use]
pub const fn self_broadcast_auto_max_fee_per_gas(rpc_gas_price: u128) -> u128 {
    rpc_gas_price.saturating_mul(SELF_BROADCAST_AUTO_MAX_FEE_NUMERATOR)
        / SELF_BROADCAST_AUTO_MAX_FEE_DENOMINATOR
}

#[must_use]
pub const fn self_broadcast_replacement_bumped_fee(value: u128) -> u128 {
    value
        .saturating_mul(SELF_BROADCAST_REPLACEMENT_BUMP_NUMERATOR)
        .saturating_add(SELF_BROADCAST_REPLACEMENT_BUMP_DENOMINATOR - 1)
        / SELF_BROADCAST_REPLACEMENT_BUMP_DENOMINATOR
}

pub async fn quote_desktop_self_broadcast_gas_fee(
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    http: &HttpContext,
) -> Result<SelfBroadcastGasFeeQuote> {
    let chain = effective_desktop_chain_config(chain_id, effective_chain)?;
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls, http);
    self_broadcast_gas_fee_quote_from_rpc_pool(&query_rpc_pool, http.network_mode()).await
}

pub struct DesktopUnshieldSelfBroadcastRequest {
    pub executor: Option<Arc<PreparedExecutorOperation>>,
    pub executor_maximum_gas: Option<u64>,
    pub transaction_tracking: Option<crate::PublicTransactionTrackingContext>,
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub spend_authorization: DesktopPrivateSpendAuthorization,
    pub vault_password: Option<Zeroizing<String>>,
    pub protected_software_seed_session: Option<Arc<vault::ProtectedSoftwareSeedSession>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
    pub token: Address,
    pub fee_token: Address,
    pub amount: U256,
    pub fee_mode: FeeHandlingMode,
    pub recipient: Address,
    pub unwrap: bool,
    pub native_top_up: Option<DesktopNativeTopUpRequest>,
    pub verify_proof: bool,
    pub gas_fee: SelfBroadcastGasFeeSelection,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
    pub command_rx: Option<SelfBroadcastCommandReceiver>,
    pub event_tx: Option<SelfBroadcastSessionEventSender>,
}

pub struct DesktopSendSelfBroadcastRequest {
    pub transaction_tracking: Option<crate::PublicTransactionTrackingContext>,
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub spend_authorization: DesktopPrivateSpendAuthorization,
    pub vault_password: Option<Zeroizing<String>>,
    pub protected_software_seed_session: Option<Arc<vault::ProtectedSoftwareSeedSession>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
    pub token: Address,
    pub fee_token: Address,
    pub amount: U256,
    pub recipient: String,
    pub verify_proof: bool,
    pub gas_fee: SelfBroadcastGasFeeSelection,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
    pub command_rx: Option<SelfBroadcastCommandReceiver>,
    pub event_tx: Option<SelfBroadcastSessionEventSender>,
}

pub struct DesktopSponsoredSendCalldataRequest {
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub spend_authorization: DesktopPrivateSpendAuthorization,
    pub public_account_uuid: String,
    pub token: Address,
    pub amount: U256,
    pub recipient: String,
    pub verify_proof: bool,
    pub gas_fee: SelfBroadcastGasFeeSelection,
    pub incentive: SponsoredIncentive,
    pub authorization_limit: SponsoredAuthorizationLimit,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
}

pub struct DesktopSponsoredUnshieldCalldataRequest {
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub spend_authorization: DesktopPrivateSpendAuthorization,
    pub public_account_uuid: String,
    pub token: Address,
    pub amount: U256,
    pub fee_mode: FeeHandlingMode,
    pub recipient: Address,
    pub unwrap: bool,
    pub native_top_up: Option<DesktopNativeTopUpRequest>,
    pub verify_proof: bool,
    pub gas_fee: SelfBroadcastGasFeeSelection,
    pub incentive: SponsoredIncentive,
    pub authorization_limit: SponsoredAuthorizationLimit,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
}

pub struct DesktopPreparedSponsoredSelfBroadcastRequest {
    pub transaction_tracking: Option<crate::PublicTransactionTrackingContext>,
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub vault_password: Option<Zeroizing<String>>,
    pub protected_software_seed_session: Option<Arc<vault::ProtectedSoftwareSeedSession>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
    pub prepared: PreparedSponsoredCall,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
    pub command_rx: watch::Receiver<SponsoredSelfBroadcastCommand>,
    pub event_tx: Option<SelfBroadcastSessionEventSender>,
}

pub struct DesktopSponsoredSendSelfBroadcastRequest {
    pub transaction_tracking: Option<crate::PublicTransactionTrackingContext>,
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub spend_authorization: DesktopPrivateSpendAuthorization,
    pub vault_password: Option<Zeroizing<String>>,
    pub protected_software_seed_session: Option<Arc<vault::ProtectedSoftwareSeedSession>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
    pub token: Address,
    pub amount: U256,
    pub recipient: String,
    pub verify_proof: bool,
    pub gas_fee: SelfBroadcastGasFeeSelection,
    pub incentive: SponsoredIncentive,
    pub authorization_limit: SponsoredAuthorizationLimit,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
    pub command_rx: watch::Receiver<SponsoredSelfBroadcastCommand>,
    pub event_tx: Option<SelfBroadcastSessionEventSender>,
}

pub struct DesktopSponsoredUnshieldSelfBroadcastRequest {
    pub transaction_tracking: Option<crate::PublicTransactionTrackingContext>,
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub spend_authorization: DesktopPrivateSpendAuthorization,
    pub vault_password: Option<Zeroizing<String>>,
    pub protected_software_seed_session: Option<Arc<vault::ProtectedSoftwareSeedSession>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub public_account_uuid: String,
    pub token: Address,
    pub amount: U256,
    pub fee_mode: FeeHandlingMode,
    pub recipient: Address,
    pub unwrap: bool,
    pub native_top_up: Option<DesktopNativeTopUpRequest>,
    pub verify_proof: bool,
    pub gas_fee: SelfBroadcastGasFeeSelection,
    pub incentive: SponsoredIncentive,
    pub authorization_limit: SponsoredAuthorizationLimit,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
    pub command_rx: watch::Receiver<SponsoredSelfBroadcastCommand>,
    pub event_tx: Option<SelfBroadcastSessionEventSender>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockedShieldRescueUtxoId {
    pub tree: u32,
    pub position: u64,
    pub commitment: FixedBytes<32>,
    pub blinded_commitment: FixedBytes<32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedShieldRescueEligibility {
    pub eligible: bool,
    pub disabled_reason: Option<String>,
    pub origin_address: Option<Address>,
    pub public_account_uuid: Option<String>,
    pub public_account_label: Option<String>,
}

pub struct BlockedShieldRescueEligibilityRequest {
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub utxo_id: BlockedShieldRescueUtxoId,
}

pub struct BlockedShieldRescuePreviewRequest {
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub utxo_id: BlockedShieldRescueUtxoId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedShieldRescuePreview {
    pub chain_id: u64,
    pub utxo_id: BlockedShieldRescueUtxoId,
    pub token: Address,
    pub amount: U256,
    pub source_tx_hash: FixedBytes<32>,
    pub origin_address: Address,
    pub public_account_uuid: String,
    pub public_account_label: Option<String>,
}

pub struct BlockedShieldRescueSelfBroadcastRequest {
    pub transaction_tracking: Option<crate::PublicTransactionTrackingContext>,
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub spend_authorization: DesktopPrivateSpendAuthorization,
    pub vault_password: Zeroizing<String>,
    pub protected_software_seed_session: Option<Arc<vault::ProtectedSoftwareSeedSession>>,
    pub trezor_pin_matrix_provider: Option<HardwareTrezorPinMatrixProvider>,
    pub utxo_id: BlockedShieldRescueUtxoId,
    pub requested_public_account_uuid: Option<String>,
    pub verify_proof: bool,
    pub gas_fee: SelfBroadcastGasFeeSelection,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
    pub command_rx: Option<SelfBroadcastCommandReceiver>,
    pub event_tx: Option<SelfBroadcastSessionEventSender>,
}
