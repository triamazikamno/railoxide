use super::*;
use alloy::uint;
use eyre::eyre;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::block_observer::resolve_transaction_sender_by_block;

#[derive(Debug, Clone)]
pub struct PublicBroadcasterCandidate {
    pub chain_id: u64,
    pub railgun_address: String,
    pub identifier: Option<String>,
    pub token: Address,
    pub fee: U256,
    pub fees_id: String,
    pub fee_expiration: SystemTime,
    pub reliability: f64,
    pub available_wallets: u32,
    pub version: String,
    pub relay_adapt: Address,
    pub relay_adapt_7702: Option<Address>,
    pub required_poi_list_keys: Vec<String>,
    pub viewing_public_key: [u8; 32],
    pub address_data: AddressData,
    pub fee_policy_status: BroadcasterFeePolicyStatus,
}

impl PublicBroadcasterCandidate {
    #[must_use]
    pub const fn is_allowed_by_fee_policy(&self, policy: BroadcasterFeePolicy) -> bool {
        policy.allows_status(self.fee_policy_status)
    }

    #[must_use]
    pub const fn is_fee_suspicious(&self) -> bool {
        self.fee_policy_status.is_suspicious()
    }

    pub fn parsed_required_poi_list_keys(&self) -> Result<Vec<FixedBytes<32>>> {
        self.required_poi_list_keys
            .iter()
            .map(|list_key| {
                let bare = list_key.strip_prefix("0x").unwrap_or(list_key);
                if bare.len() != 64 {
                    return Err(eyre!(
                        "invalid required POI list key {list_key}: expected 32-byte hex"
                    ));
                }
                let bytes = hex::decode_to_array(bare)
                    .wrap_err_with(|| format!("invalid required POI list key {list_key}"))?;
                Ok(FixedBytes::from(bytes))
            })
            .collect()
    }

    fn from_fee_row(row: &FeeRow) -> Option<Self> {
        Self::from_fee_row_with_policy_status(row, BroadcasterFeePolicyStatus::UnknownAnchor)
    }

    fn from_fee_row_with_policy_status(
        row: &FeeRow,
        fee_policy_status: BroadcasterFeePolicyStatus,
    ) -> Option<Self> {
        let railgun_address = RailgunAddress::from(row.railgun_address.as_ref());
        let address_data = AddressData::try_from(&railgun_address).ok()?;
        let candidate = Self {
            chain_id: row.chain_id,
            railgun_address: row.railgun_address.to_string(),
            identifier: row.identifier.as_ref().map(ToString::to_string),
            token: row.token_address,
            fee: row.fee,
            fees_id: row.fees_id.to_string(),
            fee_expiration: row.fee_expiration,
            reliability: row.reliability,
            available_wallets: row.available_wallets,
            version: row.version.to_string(),
            relay_adapt: row.relay_adapt,
            relay_adapt_7702: row.relay_adapt_7702,
            required_poi_list_keys: row
                .required_poi_list_keys
                .iter()
                .map(ToString::to_string)
                .collect(),
            viewing_public_key: address_data.viewing_public_key,
            address_data,
            fee_policy_status,
        };
        candidate.parsed_required_poi_list_keys().ok()?;
        Some(candidate)
    }
}

#[derive(Debug, Clone, Default)]
pub struct PublicBroadcasterTrustFilter {
    pub preferences: vault::BroadcasterPreferences,
    pub favorites_only: bool,
}

impl PublicBroadcasterTrustFilter {
    #[must_use]
    pub fn allows(&self, candidate: &PublicBroadcasterCandidate) -> bool {
        if self
            .preferences
            .banned
            .iter()
            .any(|entry| broadcaster_preference_matches_candidate(entry, candidate))
        {
            return false;
        }
        !self.favorites_only
            || self
                .preferences
                .favorites
                .iter()
                .any(|entry| broadcaster_preference_matches_candidate(entry, candidate))
    }
}

#[must_use]
pub fn filter_public_broadcasters_by_trust(
    candidates: &[PublicBroadcasterCandidate],
    trust_filter: &PublicBroadcasterTrustFilter,
) -> Vec<PublicBroadcasterCandidate> {
    candidates
        .iter()
        .filter(|candidate| trust_filter.allows(candidate))
        .cloned()
        .collect()
}

fn broadcaster_preference_matches_candidate(
    entry: &vault::BroadcasterPreferenceEntry,
    candidate: &PublicBroadcasterCandidate,
) -> bool {
    parse_railgun_recipient(&entry.address).is_ok_and(|address_data| {
        address_data.master_public_key == candidate.address_data.master_public_key
            && address_data.viewing_public_key == candidate.address_data.viewing_public_key
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicBroadcasterSelection {
    Random,
    Specific { railgun_address: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FeeHandlingMode {
    #[default]
    DeductFromAmount,
    AddToAmount,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TransactionGenerationStage {
    #[default]
    SelectingPrivateNotes,
    ProvingTransaction,
    EstimatingBroadcasterFee,
    GeneratingPoiProofs,
    PublishingToBroadcaster,
    WaitingForBroadcasterResponse,
    EstimatingSelfBroadcastGas,
    SigningSelfBroadcast,
    WaitingForSelfBroadcastReceipt,
}

impl TransactionGenerationStage {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SelectingPrivateNotes => "Selecting private notes",
            Self::ProvingTransaction => "Proving transaction",
            Self::EstimatingBroadcasterFee => "Estimating transaction fee",
            Self::GeneratingPoiProofs => "Generating POI proofs",
            Self::PublishingToBroadcaster => "Publishing to broadcaster",
            Self::WaitingForBroadcasterResponse => "Waiting for broadcaster response",
            Self::EstimatingSelfBroadcastGas => "Estimating self-broadcast gas",
            Self::SigningSelfBroadcast => "Signing self-broadcast transaction",
            Self::WaitingForSelfBroadcastReceipt => "Waiting for self-broadcast receipt",
        }
    }

    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::SelectingPrivateNotes => {
                "Finding POI-verified notes that cover the amount and fee."
            }
            Self::ProvingTransaction => {
                "Generating the zero-knowledge proof. This is usually the slowest step."
            }
            Self::EstimatingBroadcasterFee => "Checking gas cost and transaction fee requirements.",
            Self::GeneratingPoiProofs => "Generating POI proofs for transaction outputs.",
            Self::PublishingToBroadcaster => "Encrypting and publishing the request over Waku.",
            Self::WaitingForBroadcasterResponse => {
                "Waiting for the selected broadcaster to respond."
            }
            Self::EstimatingSelfBroadcastGas => {
                "Estimating direct transaction gas and checking the gas payer balance."
            }
            Self::SigningSelfBroadcast => "Unlocking the selected Public account and signing.",
            Self::WaitingForSelfBroadcastReceipt => {
                "Waiting for the submitted transaction receipt."
            }
        }
    }
}

pub type TransactionGenerationProgressSender = watch::Sender<TransactionGenerationStage>;

pub(super) fn update_transaction_generation_stage(
    progress_tx: Option<&TransactionGenerationProgressSender>,
    stage: TransactionGenerationStage,
) {
    if let Some(progress_tx) = progress_tx {
        let _ = progress_tx.send(stage);
    }
}

pub struct DesktopUnshieldPublicBroadcasterRequest {
    pub chain_id: u64,
    pub effective_chain: Option<settings::EffectiveChainConfig>,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub spend_authorization: DesktopPrivateSpendAuthorization,
    pub token: Address,
    pub fee_token: Address,
    pub amount: U256,
    pub recipient: Address,
    pub unwrap: bool,
    pub native_top_up: Option<DesktopNativeTopUpRequest>,
    pub verify_proof: bool,
    pub fee_rows: Vec<FeeRow>,
    pub selection: PublicBroadcasterSelection,
    pub fee_mode: FeeHandlingMode,
    pub fee_policy: BroadcasterFeePolicy,
    pub trust_filter: PublicBroadcasterTrustFilter,
    pub anchor_cache: Option<Arc<TokenAnchorRateCache>>,
    pub waku: Arc<WakuClient>,
    pub response_timeout: Duration,
    pub republish_interval: Duration,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
}

pub struct DesktopSendPublicBroadcasterRequest {
    pub chain_id: u64,
    pub effective_chain: Option<settings::EffectiveChainConfig>,
    pub view_session: Arc<vault::DesktopViewSession>,
    pub session: Arc<WalletSession>,
    pub vault_store: Arc<vault::DesktopVaultStore>,
    pub spend_authorization: DesktopPrivateSpendAuthorization,
    pub token: Address,
    pub fee_token: Address,
    pub amount: U256,
    pub recipient: String,
    pub verify_proof: bool,
    pub fee_rows: Vec<FeeRow>,
    pub selection: PublicBroadcasterSelection,
    pub fee_mode: FeeHandlingMode,
    pub fee_policy: BroadcasterFeePolicy,
    pub trust_filter: PublicBroadcasterTrustFilter,
    pub anchor_cache: Option<Arc<TokenAnchorRateCache>>,
    pub waku: Arc<WakuClient>,
    pub response_timeout: Duration,
    pub republish_interval: Duration,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
}

pub struct DesktopUnshieldPublicBroadcasterEstimateRequest {
    pub chain_id: u64,
    pub effective_chain: Option<settings::EffectiveChainConfig>,
    pub session: Arc<WalletSession>,
    pub token: Address,
    pub fee_token: Address,
    pub amount: U256,
    pub recipient: Address,
    pub unwrap: bool,
    pub native_top_up: Option<DesktopNativeTopUpRequest>,
    pub fee_rows: Vec<FeeRow>,
    pub selection: PublicBroadcasterSelection,
    pub fee_mode: FeeHandlingMode,
    pub fee_policy: BroadcasterFeePolicy,
    pub trust_filter: PublicBroadcasterTrustFilter,
    pub anchor_cache: Option<Arc<TokenAnchorRateCache>>,
}

pub struct DesktopSendPublicBroadcasterEstimateRequest {
    pub chain_id: u64,
    pub effective_chain: Option<settings::EffectiveChainConfig>,
    pub session: Arc<WalletSession>,
    pub token: Address,
    pub fee_token: Address,
    pub amount: U256,
    pub recipient: String,
    pub fee_rows: Vec<FeeRow>,
    pub selection: PublicBroadcasterSelection,
    pub fee_mode: FeeHandlingMode,
    pub fee_policy: BroadcasterFeePolicy,
    pub trust_filter: PublicBroadcasterTrustFilter,
    pub anchor_cache: Option<Arc<TokenAnchorRateCache>>,
}

#[derive(Debug, Clone)]
pub struct PublicBroadcasterCostEstimate {
    pub broadcaster: PublicBroadcasterCandidate,
    pub action_token: Address,
    pub fee_token: Address,
    pub entered_amount: U256,
    pub receiver_amount: U256,
    pub recipient_amount: U256,
    pub total_private_spend: U256,
    pub fee_amount: U256,
    pub protocol_fee_amount: U256,
    pub protocol_fee_bps: U256,
    pub fee_mode: FeeHandlingMode,
    pub max_receiver_amount: U256,
    pub max_entered_amount: U256,
    pub gas_limit: u64,
    pub min_gas_price: u128,
    pub native_gas_cost: U256,
    pub transaction_count: usize,
    pub input_count: usize,
    pub private_output_count: usize,
    pub public_output_count: usize,
    pub relay_call_count: usize,
    pub uses_relay_adapt: bool,
    pub native_top_up: Option<DesktopNativeTopUpPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicBroadcasterResultKind {
    Submitted { tx_hash: String },
    Failed { error: String },
    TimedOut,
}

#[derive(Debug, Clone)]
pub struct PublicBroadcasterSubmissionResult {
    pub broadcaster: PublicBroadcasterCandidate,
    pub action_token: Address,
    pub fee_token: Address,
    pub entered_amount: U256,
    pub receiver_amount: U256,
    pub recipient_amount: U256,
    pub total_private_spend: U256,
    pub fee_amount: U256,
    pub protocol_fee_amount: U256,
    pub protocol_fee_bps: U256,
    pub fee_mode: FeeHandlingMode,
    pub gas_limit: u64,
    pub min_gas_price: u128,
    pub transaction_count: usize,
    pub input_count: usize,
    pub private_output_count: usize,
    pub public_output_count: usize,
    pub relay_call_count: usize,
    pub uses_relay_adapt: bool,
    pub result: PublicBroadcasterResultKind,
    pub native_top_up: Option<DesktopNativeTopUpPlan>,
}

#[derive(Debug, Clone)]
pub(super) struct PreparedPublicBroadcasterPlan<P> {
    pub(super) plan: P,
    pub(super) pre_transaction_pois_per_txid_leaf_per_list: PreTransactionPoiMap,
    pub(super) broadcaster: PublicBroadcasterCandidate,
    pub(super) action_token: Address,
    pub(super) fee_token: Address,
    pub(super) entered_amount: U256,
    pub(super) receiver_amount: U256,
    pub(super) recipient_amount: U256,
    pub(super) total_private_spend: U256,
    pub(super) fee_amount: U256,
    pub(super) protocol_fee_amount: U256,
    pub(super) protocol_fee_bps: U256,
    pub(super) fee_mode: FeeHandlingMode,
    pub(super) gas_limit: u64,
    pub(super) min_gas_price: u128,
    pub(super) bound_min_gas_price: u128,
    pub(super) transaction_count: usize,
    pub(super) input_count: usize,
    pub(super) private_output_count: usize,
    pub(super) public_output_count: usize,
    pub(super) relay_call_count: usize,
    pub(super) uses_relay_adapt: bool,
    pub(super) native_top_up: Option<DesktopNativeTopUpPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedUnshieldCall {
    pub chain_id: u64,
    pub token: Address,
    pub amount: U256,
    pub fee_mode: FeeHandlingMode,
    pub recipient: Address,
    pub unwrap: bool,
    pub max_spendable: U256,
    pub transaction_count: usize,
    pub input_count: usize,
    pub private_output_count: usize,
    pub public_output_count: usize,
    pub to: Address,
    pub data: String,
    pub native_top_up: Option<DesktopNativeTopUpPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSendCall {
    pub chain_id: u64,
    pub token: Address,
    pub amount: U256,
    pub recipient: String,
    pub max_spendable: U256,
    pub transaction_count: usize,
    pub input_count: usize,
    pub private_output_count: usize,
    pub public_output_count: usize,
    pub to: Address,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSponsoredCall {
    pub chain_id: u64,
    pub action: SponsoredActionKind,
    pub authorization: SponsoredAuthorization,
    pub transaction_count: usize,
    pub input_count: usize,
    pub private_output_count: usize,
    pub public_output_count: usize,
    pub relay_call_count: usize,
    pub uses_relay_adapt: bool,
    pub selected_inputs: Vec<SelectedInputIdentity>,
    pub native_top_up: Option<DesktopNativeTopUpPlan>,
    pub total_wrapped_native_spend: U256,
    pub to: Address,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopSponsoredSelfBroadcastResult {
    pub prepared: PreparedSponsoredCall,
    pub outcome: SponsoredSelfBroadcastSessionOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopSelfBroadcastResult {
    pub chain_id: u64,
    pub public_account_uuid: String,
    pub gas_payer: Address,
    pub gas_limit: u64,
    pub rpc_gas_price: u128,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub estimated_native_gas_cost: U256,
    pub live_native_balance: U256,
    pub tx: TxReceiptOutput,
    pub attempts: Vec<SelfBroadcastAttemptInfo>,
    pub native_top_up: Option<DesktopNativeTopUpPlan>,
}

pub(super) struct PreparedPrivatePlan<P> {
    pub(super) plan: P,
    pub(super) max_spendable: U256,
    pub(super) prover: ProverService,
}

pub(super) struct PreparedDesktopUnshieldPlan {
    pub(super) plan: DesktopUnshieldPreparedPlan,
    pub(super) max_spendable: U256,
    pub(super) prover: ProverService,
    pub(super) native_top_up: Option<DesktopNativeTopUpPlan>,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub(super) enum DesktopUnshieldPreparedPlan {
    Single(UnshieldPlan),
    Composite(CompositeUnshieldPlan),
}

impl DesktopUnshieldPreparedPlan {
    pub(super) fn chunks(&self) -> &[TransactionPlanChunk] {
        match self {
            Self::Single(plan) => &plan.chunks,
            Self::Composite(plan) => &plan.chunks,
        }
    }

    pub(super) fn input_utxos(&self) -> Vec<Utxo> {
        match self {
            Self::Single(plan) => plan.inputs.iter().map(|input| input.utxo.clone()).collect(),
            Self::Composite(plan) => plan.inputs.iter().map(|input| input.utxo.clone()).collect(),
        }
    }

    pub(super) const fn call_to(&self) -> Address {
        match self {
            Self::Single(plan) => plan.call.to,
            Self::Composite(plan) => plan.call.to,
        }
    }

    pub(super) fn call_data(&self) -> Bytes {
        match self {
            Self::Single(plan) => plan.call.data.clone(),
            Self::Composite(plan) => plan.call.data.clone(),
        }
    }

    pub(super) const fn transaction_count(&self) -> usize {
        match self {
            Self::Single(plan) => plan.transaction_count(),
            Self::Composite(plan) => plan.shape.transaction_count,
        }
    }

    pub(super) const fn input_count(&self) -> usize {
        match self {
            Self::Single(plan) => plan.input_count(),
            Self::Composite(plan) => plan.shape.input_count,
        }
    }

    pub(super) const fn private_output_count(&self) -> usize {
        match self {
            Self::Single(plan) => plan.private_output_count(),
            Self::Composite(plan) => plan.shape.private_output_count,
        }
    }

    pub(super) const fn public_output_count(&self) -> usize {
        match self {
            Self::Single(plan) => plan.public_output_count(),
            Self::Composite(plan) => plan.shape.public_output_count,
        }
    }
}

pub(super) struct PreparedBlockedShieldRescuePlan {
    pub(super) plan: UnshieldPlan,
    pub(super) public_account_uuid: String,
}

pub(super) struct DesktopUnshieldPlanRequest<'a> {
    pub(super) chain_id: u64,
    pub(super) effective_chain: Option<&'a settings::EffectiveChainConfig>,
    pub(super) view_session: &'a vault::DesktopViewSession,
    pub(super) session: &'a WalletSession,
    pub(super) vault_store: &'a vault::DesktopVaultStore,
    pub(super) spend_authorization: DesktopPrivateSpendAuthorization,
    pub(super) token: Address,
    pub(super) amount: U256,
    pub(super) fee_mode: FeeHandlingMode,
    pub(super) recipient: Address,
    pub(super) unwrap: bool,
    pub(super) native_top_up: Option<DesktopNativeTopUpRequest>,
    pub(super) verify_proof: bool,
    pub(super) progress_tx: Option<&'a TransactionGenerationProgressSender>,
}

pub(super) struct DesktopSendPlanRequest<'a> {
    pub(super) chain_id: u64,
    pub(super) effective_chain: Option<&'a settings::EffectiveChainConfig>,
    pub(super) view_session: &'a vault::DesktopViewSession,
    pub(super) session: &'a WalletSession,
    pub(super) vault_store: &'a vault::DesktopVaultStore,
    pub(super) spend_authorization: DesktopPrivateSpendAuthorization,
    pub(super) token: Address,
    pub(super) amount: U256,
    pub(super) recipient: &'a str,
    pub(super) verify_proof: bool,
    pub(super) progress_tx: Option<&'a TransactionGenerationProgressSender>,
}

pub(super) struct SelfBroadcastPreflight {
    pub(super) tx_req: TransactionRequest,
    pub(super) nonce: u64,
    pub(super) gas_limit: u64,
    pub(super) rpc_gas_price: u128,
    pub(super) max_fee_per_gas: u128,
    pub(super) max_priority_fee_per_gas: u128,
    pub(super) estimated_native_gas_cost: U256,
    pub(super) live_native_balance: U256,
}

pub(super) struct SubmittedSelfBroadcastAttempt {
    pub(super) tx_hash: FixedBytes<32>,
    pub(super) info: SelfBroadcastAttemptInfo,
    pub(super) rpc_gas_price: u128,
    pub(super) estimated_native_gas_cost: U256,
    pub(super) live_native_balance: U256,
}

pub(super) struct SelfBroadcastSentTx {
    pub(super) tx_hash: FixedBytes<32>,
    pub(super) tx_hash_string: String,
}

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalletSyncTip {
    pub last_scanned_block: Option<u64>,
    pub head_block: Option<u64>,
    pub safe_head_block: Option<u64>,
    pub head_last_advanced_at_unix_secs: Option<u64>,
    pub indexed_catch_up: Option<WalletIndexedCatchUpStatus>,
}

#[derive(Clone)]
pub struct WalletSessionObservation {
    pub snapshot: Arc<ListUtxosOutput>,
    pub readiness: WalletReadiness,
    pub ppoi_workflow_status: WalletPpoiWorkflowStatus,
}

pub struct WalletSession {
    pub chain_id: u64,
    pub poi_read_source: PoiReadSource,
    pub cache_key: String,
    pub start_block: u64,
    pub observation_rx: watch::Receiver<WalletSessionObservation>,
    pub sync_tip_rx: watch::Receiver<WalletSyncTip>,
    pub poi_refreshing_rx: watch::Receiver<bool>,
    pub poi_artifact_cache_progress_rx:
        Option<watch::Receiver<BTreeMap<u64, PoiArtifactCacheProgress>>>,
    pub(crate) db: Arc<DbStore>,
    pub(crate) sync_manager: Arc<SyncManager>,
    pub(crate) chain_key: ChainKey,
    pub(crate) handle: WalletHandle,
    pub(crate) public_data_plane: PublicDataPlaneHandle,
    pub(super) projection_cancel_tx: watch::Sender<bool>,
    pub(super) projection_join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

pub struct WalletPoiArtifactCacheRetry {
    retry: CorePoiArtifactCacheRetry,
    handle: WalletHandle,
}

impl WalletPoiArtifactCacheRetry {
    #[must_use]
    pub const fn attempt_id(&self) -> PoiArtifactCacheAttemptId {
        self.retry.attempt_id()
    }

    pub async fn wait(self) -> Result<bool> {
        self.retry
            .wait()
            .await
            .wrap_err("retry chain PPOI corpus refresh")?;
        Ok(self.handle.refresh_poi_statuses().await)
    }
}

impl WalletSession {
    pub async fn stop(&self) -> Result<()> {
        let result = self
            .sync_manager
            .remove_wallet_session(&self.handle)
            .await
            .wrap_err("remove wallet sync worker");
        if result.is_err() {
            let _ = self.projection_cancel_tx.send(true);
        }
        let projection_join = match self.projection_join.lock() {
            Ok(mut projection_join) => projection_join.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(projection_join) = projection_join {
            projection_join
                .await
                .wrap_err("join wallet session observation projection")?;
        }
        result
    }

    #[must_use]
    pub fn unspent_utxos(&self) -> Vec<Utxo> {
        let Some(snapshot) = self.handle.current_snapshot() else {
            return Vec::new();
        };
        poi_verified_unspent_utxos_from_records(&snapshot.utxos, &snapshot.pending_overlay)
    }

    pub(crate) async fn mark_pending_spent_utxos(
        &self,
        utxos: &[Utxo],
        tx_hash: Option<FixedBytes<32>>,
    ) {
        match self.handle.mark_pending_spent_utxos(utxos, tx_hash).await {
            Ok(
                WalletPendingSpentMarkOutcome::Marked
                | WalletPendingSpentMarkOutcome::AlreadyProtected,
            ) => {}
            Err(error) => {
                tracing::warn!(%error, "wallet actor rejected local pending-spend update");
            }
        }
    }

    pub(crate) async fn renew_pending_spent_utxos(
        &self,
        utxos: &[Utxo],
        tx_hash: FixedBytes<32>,
    ) -> Result<()> {
        self.handle
            .mark_pending_spent_utxos(utxos, Some(tx_hash))
            .await
            .map(|_| ())
            .map_err(Report::new)
            .wrap_err("renew local pending-spend protection")
    }

    pub async fn clear_local_pending_spent(&self) -> bool {
        self.handle
            .clear_all_local_pending_spent()
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "wallet actor rejected local pending-spend clear");
                false
            })
    }

    pub async fn refresh_poi_statuses(&self) -> bool {
        self.handle.refresh_poi_statuses().await
    }

    pub async fn retry_poi_artifact_cache(&self) -> Result<WalletPoiArtifactCacheRetry> {
        let retry = self
            .public_data_plane
            .retry_poi_artifact_cache()
            .await
            .wrap_err("retry chain PPOI corpus refresh")?;
        Ok(WalletPoiArtifactCacheRetry {
            retry,
            handle: self.handle.clone(),
        })
    }
}

impl Drop for WalletSession {
    fn drop(&mut self) {
        let _ = self.projection_cancel_tx.send(true);
        if let Ok(projection_join) = self.projection_join.get_mut()
            && let Some(projection_join) = projection_join.take()
        {
            projection_join.abort();
        }
    }
}

pub(crate) fn poi_verified_unspent_utxos_from_records(
    utxos: &[WalletUtxo],
    pending_overlay: &WalletPendingOverlay,
) -> Vec<Utxo> {
    let active_poi_list_keys = default_active_poi_list_keys();
    let chain_pending_spent_keys = chain_pending_spent_keys(pending_overlay);
    utxos
        .iter()
        .filter(|entry| !entry.is_spent())
        .filter(|entry| {
            !chain_pending_spent_keys.contains(&(entry.utxo.tree, entry.utxo.position))
                && !pending_overlay
                    .local_pending_spent
                    .iter()
                    .any(|spent| spent.matches_local_utxo(entry))
        })
        .filter(|entry| entry.utxo.poi.is_valid_for_lists(&active_poi_list_keys))
        .map(|entry| entry.utxo.clone())
        .collect()
}

pub(super) fn chain_pending_spent_keys(
    pending_overlay: &WalletPendingOverlay,
) -> HashSet<(u32, u64)> {
    pending_overlay
        .pending_spent
        .iter()
        .map(WalletPendingSpent::key)
        .collect()
}

pub async fn resolve_blocked_shield_rescue_eligibility(
    request: BlockedShieldRescueEligibilityRequest,
    http: &HttpContext,
) -> Result<BlockedShieldRescueEligibility> {
    let Some(snapshot) = request.session.handle.current_snapshot() else {
        return Ok(blocked_shield_rescue_disabled(
            "Wallet state is temporarily unavailable while synchronization resets.",
            None,
        ));
    };
    let Some(utxo) = blocked_shield_rescue_candidate_from_records(
        &snapshot.utxos,
        &snapshot.pending_overlay,
        &request.utxo_id,
    ) else {
        return Ok(blocked_shield_rescue_disabled(
            "Selected UTXO is not an unspent blocked Shield that can be refunded.",
            None,
        ));
    };

    let origin = match resolve_source_tx_origin(
        request.chain_id,
        request.effective_chain.as_ref(),
        utxo.source.block_number,
        utxo.source.tx_hash,
        http,
    )
    .await
    {
        Ok(origin) => origin,
        Err(error) => {
            tracing::warn!(%error, "resolve blocked Shield source origin failed");
            return Ok(blocked_shield_rescue_disabled(
                "Source transaction origin could not be resolved. Retry after checking RPC connectivity.",
                None,
            ));
        }
    };

    let accounts = request
        .vault_store
        .list_active_public_accounts_for_session(&request.view_session)
        .wrap_err("load active public accounts")?;
    Ok(blocked_shield_rescue_eligibility_for_origin(
        Some(origin),
        &accounts,
    ))
}

pub async fn resolve_source_tx_origin(
    chain_id: u64,
    effective_chain: Option<&settings::EffectiveChainConfig>,
    source_block_number: u64,
    source_tx_hash: FixedBytes<32>,
    http: &HttpContext,
) -> Result<Address> {
    let chain = effective_desktop_chain_config(chain_id, effective_chain)?;
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls, http);
    resolve_transaction_sender_by_block(&query_rpc_pool, source_block_number, source_tx_hash).await
}

pub(crate) fn blocked_shield_rescue_candidate_from_records(
    utxos: &[WalletUtxo],
    pending_overlay: &WalletPendingOverlay,
    utxo_id: &BlockedShieldRescueUtxoId,
) -> Option<Utxo> {
    let chain_pending_spent_keys = chain_pending_spent_keys(pending_overlay);
    let active_poi_list_keys = default_active_poi_list_keys();
    utxos
        .iter()
        .filter(|entry| !entry.is_spent())
        .filter(|entry| {
            !chain_pending_spent_keys.contains(&(entry.utxo.tree, entry.utxo.position))
                && !pending_overlay
                    .local_pending_spent
                    .iter()
                    .any(|spent| spent.matches_local_utxo(entry))
        })
        .find(|entry| blocked_shield_rescue_utxo_matches(&entry.utxo, utxo_id))
        .filter(|entry| {
            utxos::activity_utxo_classification(&entry.utxo.poi, &active_poi_list_keys)
                == ActivityUtxoClassification::BlockedShield
        })
        .map(|entry| entry.utxo.clone())
}

pub(super) fn blocked_shield_rescue_utxo_matches(
    utxo: &Utxo,
    utxo_id: &BlockedShieldRescueUtxoId,
) -> bool {
    utxo.tree == utxo_id.tree
        && utxo.position == utxo_id.position
        && utxo.poi.commitment == utxo_id.commitment
        && utxo.poi.blinded_commitment == utxo_id.blinded_commitment
}

pub(crate) fn blocked_shield_rescue_eligibility_for_origin(
    origin: Option<Address>,
    active_public_accounts: &[vault::PublicAccountMetadata],
) -> BlockedShieldRescueEligibility {
    let Some(origin) = origin else {
        return blocked_shield_rescue_disabled(
            "Source transaction origin could not be resolved. Retry after checking RPC connectivity.",
            None,
        );
    };
    let Some(account) = active_public_accounts.iter().find(|account| {
        account.address == origin && account.status == vault::PublicAccountStatus::Active
    }) else {
        return blocked_shield_rescue_disabled(
            "The Shield origin Public account must be added or activated before refund.",
            Some(origin),
        );
    };

    BlockedShieldRescueEligibility {
        eligible: true,
        disabled_reason: None,
        origin_address: Some(origin),
        public_account_uuid: Some(account.public_account_uuid.clone()),
        public_account_label: account.label.clone(),
    }
}

pub(super) fn blocked_shield_rescue_disabled(
    reason: &str,
    origin_address: Option<Address>,
) -> BlockedShieldRescueEligibility {
    BlockedShieldRescueEligibility {
        eligible: false,
        disabled_reason: Some(reason.to_string()),
        origin_address,
        public_account_uuid: None,
        public_account_label: None,
    }
}

pub fn eligible_public_broadcasters_for_asset(
    rows: &[FeeRow],
    chain_id: u64,
    token: Address,
    required_relay_adapt: Option<Address>,
) -> Result<Vec<PublicBroadcasterCandidate>> {
    chain_defaults_for_chain(chain_id)?;
    Ok(eligible_public_broadcasters(
        rows,
        chain_id,
        token,
        required_relay_adapt,
        SystemTime::now(),
    ))
}

pub fn public_broadcaster_candidates_for_asset(
    rows: &[FeeRow],
    chain_id: u64,
    token: Address,
    required_relay_adapt: Option<Address>,
    policy: BroadcasterFeePolicy,
    anchor_rate: Option<U256>,
) -> Result<Vec<PublicBroadcasterCandidate>> {
    chain_defaults_for_chain(chain_id)?;
    Ok(public_broadcaster_candidates(
        rows,
        chain_id,
        token,
        required_relay_adapt,
        SystemTime::now(),
        policy,
        anchor_rate,
    ))
}

#[must_use]
pub fn eligible_public_broadcasters(
    rows: &[FeeRow],
    chain_id: u64,
    token: Address,
    required_relay_adapt: Option<Address>,
    now: SystemTime,
) -> Vec<PublicBroadcasterCandidate> {
    let candidates = rows
        .iter()
        .filter(|row| row.chain_id == chain_id)
        .filter(|row| row.token_address == token)
        .filter(|row| row.signature_valid)
        .filter(|row| row.fee_expiration > now)
        .filter(|row| row.available_wallets > 0)
        .filter(|row| supported_broadcaster_version(&row.version))
        .filter(|row| required_relay_adapt.is_none_or(|relay| row.relay_adapt == relay))
        .filter_map(PublicBroadcasterCandidate::from_fee_row)
        .collect::<Vec<_>>();
    deduplicate_public_broadcasters(candidates)
}

#[must_use]
pub fn public_broadcaster_candidates(
    rows: &[FeeRow],
    chain_id: u64,
    token: Address,
    required_relay_adapt: Option<Address>,
    now: SystemTime,
    policy: BroadcasterFeePolicy,
    anchor_rate: Option<U256>,
) -> Vec<PublicBroadcasterCandidate> {
    let candidates = rows
        .iter()
        .filter(|row| row.chain_id == chain_id)
        .filter(|row| row.token_address == token)
        .filter(|row| row.signature_valid)
        .filter(|row| row.fee_expiration > now)
        .filter(|row| row.available_wallets > 0)
        .filter(|row| supported_broadcaster_version(&row.version))
        .filter(|row| required_relay_adapt.is_none_or(|relay| row.relay_adapt == relay))
        .filter_map(|row| {
            PublicBroadcasterCandidate::from_fee_row_with_policy_status(
                row,
                policy.classify_fee(row.fee, anchor_rate),
            )
        })
        .collect::<Vec<_>>();
    deduplicate_public_broadcasters(candidates)
}

fn deduplicate_public_broadcasters(
    candidates: Vec<PublicBroadcasterCandidate>,
) -> Vec<PublicBroadcasterCandidate> {
    let mut unique: Vec<PublicBroadcasterCandidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if let Some(existing) = unique.iter_mut().find(|existing| {
            existing.chain_id == candidate.chain_id
                && existing.token == candidate.token
                && existing.address_data.master_public_key
                    == candidate.address_data.master_public_key
                && existing.address_data.viewing_public_key
                    == candidate.address_data.viewing_public_key
        }) {
            if public_broadcaster_is_preferred(&candidate, existing) {
                *existing = candidate;
            }
        } else {
            unique.push(candidate);
        }
    }
    unique
}

fn public_broadcaster_is_preferred(
    candidate: &PublicBroadcasterCandidate,
    existing: &PublicBroadcasterCandidate,
) -> bool {
    candidate
        .fee_expiration
        .cmp(&existing.fee_expiration)
        .then_with(|| candidate.fees_id.cmp(&existing.fees_id))
        .then_with(|| candidate.railgun_address.cmp(&existing.railgun_address))
        .then_with(|| candidate.identifier.cmp(&existing.identifier))
        .then_with(|| candidate.fee.cmp(&existing.fee))
        .then_with(|| {
            candidate
                .required_poi_list_keys
                .cmp(&existing.required_poi_list_keys)
        })
        .then_with(|| candidate.version.cmp(&existing.version))
        .then_with(|| candidate.reliability.total_cmp(&existing.reliability))
        .then_with(|| candidate.available_wallets.cmp(&existing.available_wallets))
        .then_with(|| candidate.relay_adapt.cmp(&existing.relay_adapt))
        .then_with(|| candidate.relay_adapt_7702.cmp(&existing.relay_adapt_7702))
        .is_gt()
}

#[must_use]
pub(crate) fn random_eligible_public_broadcasters(
    candidates: &[PublicBroadcasterCandidate],
    policy: BroadcasterFeePolicy,
    trust_filter: &PublicBroadcasterTrustFilter,
) -> Vec<PublicBroadcasterCandidate> {
    let eligible = candidates
        .iter()
        .filter(|candidate| trust_filter.allows(candidate))
        .filter(|candidate| candidate.is_allowed_by_fee_policy(policy))
        .filter(|candidate| candidate.parsed_required_poi_list_keys().is_ok())
        .cloned()
        .collect();
    deduplicate_public_broadcasters(eligible)
}

#[must_use]
pub fn fee_policy_eligible_public_broadcasters(
    candidates: &[PublicBroadcasterCandidate],
    policy: BroadcasterFeePolicy,
) -> Vec<PublicBroadcasterCandidate> {
    candidates
        .iter()
        .filter(|candidate| candidate.is_allowed_by_fee_policy(policy))
        .cloned()
        .collect()
}

#[must_use]
pub fn sort_specific_public_broadcasters(
    mut candidates: Vec<PublicBroadcasterCandidate>,
    sort_seed: &[u8; 32],
) -> Vec<PublicBroadcasterCandidate> {
    candidates.sort_by(|a, b| {
        a.fee
            .cmp(&b.fee)
            .then_with(|| {
                seeded_broadcaster_sort_key(sort_seed, &a.railgun_address)
                    .cmp(&seeded_broadcaster_sort_key(sort_seed, &b.railgun_address))
            })
            .then_with(|| a.railgun_address.cmp(&b.railgun_address))
    });
    candidates
}

fn seeded_broadcaster_sort_key(sort_seed: &[u8; 32], railgun_address: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(sort_seed);
    hasher.update(railgun_address.as_bytes());
    hasher.finalize().into()
}

pub fn select_public_broadcaster(
    candidates: &[PublicBroadcasterCandidate],
    selection: &PublicBroadcasterSelection,
) -> Result<PublicBroadcasterCandidate> {
    select_public_broadcaster_with_policy(
        candidates,
        selection,
        BroadcasterFeePolicy::default().with_allow_suspicious_broadcasters(true),
    )
}

pub fn select_public_broadcaster_with_policy(
    candidates: &[PublicBroadcasterCandidate],
    selection: &PublicBroadcasterSelection,
    policy: BroadcasterFeePolicy,
) -> Result<PublicBroadcasterCandidate> {
    select_public_broadcaster_with_policy_and_trust(
        candidates,
        selection,
        policy,
        &PublicBroadcasterTrustFilter::default(),
    )
}

pub fn select_public_broadcaster_with_policy_and_trust(
    candidates: &[PublicBroadcasterCandidate],
    selection: &PublicBroadcasterSelection,
    policy: BroadcasterFeePolicy,
    trust_filter: &PublicBroadcasterTrustFilter,
) -> Result<PublicBroadcasterCandidate> {
    match selection {
        PublicBroadcasterSelection::Random => {
            let eligible_candidates =
                random_eligible_public_broadcasters(candidates, policy, trust_filter);
            let selected = eligible_candidates.choose(&mut rand::rng()).cloned();
            selected.ok_or_else(|| eyre!("no eligible public broadcaster for selected token"))
        }
        PublicBroadcasterSelection::Specific { railgun_address } => {
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.railgun_address == *railgun_address)
                .cloned()
                .ok_or_else(|| eyre!("selected public broadcaster is no longer eligible"))?;
            if !trust_filter.allows(&candidate) {
                return Err(eyre!(
                    "selected public broadcaster is excluded by current preferences"
                ));
            }
            if candidate.is_allowed_by_fee_policy(policy) {
                Ok(candidate)
            } else {
                Err(eyre!(
                    "selected public broadcaster fee is outside the allowed range"
                ))
            }
        }
    }
}

#[must_use]
pub fn broadcaster_fee_amount(
    token_fee_per_unit_gas: U256,
    gas_limit: u64,
    gas_price: u128,
) -> U256 {
    const FEE_SCALE: U256 = uint!(1_000_000_000_000_000_000_U256);
    token_fee_per_unit_gas * U256::from(gas_limit) * U256::from(gas_price) / FEE_SCALE
}

#[must_use]
pub const fn public_broadcaster_service_gas_price(min_gas_price: u128) -> u128 {
    min_gas_price * 101 / 100
}

#[must_use]
pub const fn public_broadcaster_bound_min_gas_price(chain_id: u64, min_gas_price: u128) -> u128 {
    match chain_id {
        // Arbitrum eth_call/eth_estimateGas exposes the current child-chain gas price as
        // tx.gasprice, even when a higher legacy gasPrice is supplied. The RAILGUN contract
        // documents minGasPrice as type-0-only, so bind zero on Arbitrum and keep pricing separate.
        42_161 | 42_170 | 421_614 => 0,
        _ => min_gas_price,
    }
}

#[must_use]
pub fn public_broadcaster_native_gas_cost(gas_limit: u64, min_gas_price: u128) -> U256 {
    U256::from(gas_limit) * U256::from(public_broadcaster_service_gas_price(min_gas_price))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicBroadcasterFeeMargin {
    Zero,
    Positive(U256),
    Negative(U256),
}

impl PublicBroadcasterFeeMargin {
    #[must_use]
    pub fn from_total_and_gas(total_fee: U256, gas_cost: U256) -> Self {
        if total_fee >= gas_cost {
            let margin = total_fee - gas_cost;
            if margin.is_zero() {
                Self::Zero
            } else {
                Self::Positive(margin)
            }
        } else {
            Self::Negative(gas_cost - total_fee)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicBroadcasterFeeBreakdown {
    pub native_gas_cost: U256,
    pub fee_token_gas_cost: Option<U256>,
    pub broadcaster_fee: Option<PublicBroadcasterFeeMargin>,
}

#[must_use]
pub fn public_broadcaster_fee_breakdown(
    total_fee: U256,
    gas_limit: u64,
    min_gas_price: u128,
    fee_token_anchor_rate: Option<U256>,
) -> PublicBroadcasterFeeBreakdown {
    let service_gas_price = public_broadcaster_service_gas_price(min_gas_price);
    let fee_token_gas_cost = fee_token_anchor_rate
        .filter(|anchor_rate| !anchor_rate.is_zero())
        .map(|anchor_rate| broadcaster_fee_amount(anchor_rate, gas_limit, service_gas_price));
    PublicBroadcasterFeeBreakdown {
        native_gas_cost: U256::from(gas_limit) * U256::from(service_gas_price),
        fee_token_gas_cost,
        broadcaster_fee: fee_token_gas_cost
            .map(|gas_cost| PublicBroadcasterFeeMargin::from_total_and_gas(total_fee, gas_cost)),
    }
}

pub(crate) fn broadcaster_fee_covers(available_fee: U256, required_fee: U256) -> bool {
    available_fee >= required_fee
}

#[must_use]
pub fn buffered_public_broadcaster_fee(required_fee: U256) -> U256 {
    let buffer = required_fee / PUBLIC_BROADCASTER_FEE_BUFFER_DIVISOR;
    required_fee
        + if buffer.is_zero() {
            uint!(1_U256)
        } else {
            buffer
        }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ApproximateTransactionShape {
    pub(crate) transaction_count: usize,
    pub(crate) input_count: usize,
    pub(crate) private_output_count: usize,
    pub(crate) public_output_count: usize,
    pub(crate) max_receiver_amount: U256,
    pub(crate) relay_call_count: usize,
    pub(crate) uses_relay_adapt: bool,
    pub(crate) unwrap_count: usize,
    pub(crate) send: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PublicBroadcasterAmountSplit {
    pub(crate) entered_amount: U256,
    pub(crate) receiver_amount: U256,
    pub(crate) total_private_spend: U256,
    pub(crate) fee_amount: U256,
    pub(crate) fee_mode: FeeHandlingMode,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PublicBroadcasterReportedAmounts {
    pub(crate) recipient_amount: U256,
    pub(crate) total_private_spend: U256,
    pub(crate) protocol_fee_amount: U256,
}

pub(crate) fn public_broadcaster_amount_split(
    entered_amount: U256,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
) -> Result<PublicBroadcasterAmountSplit> {
    let (receiver_amount, total_private_spend) = match fee_mode {
        FeeHandlingMode::DeductFromAmount => {
            if entered_amount <= fee_amount {
                return Err(eyre!(
                    "entered amount must be greater than the broadcaster fee"
                ));
            }
            (entered_amount - fee_amount, entered_amount)
        }
        FeeHandlingMode::AddToAmount => (entered_amount, entered_amount + fee_amount),
    };
    Ok(PublicBroadcasterAmountSplit {
        entered_amount,
        receiver_amount,
        total_private_spend,
        fee_amount,
        fee_mode,
    })
}

pub(crate) fn public_broadcaster_amount_split_for_tokens(
    entered_amount: U256,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
    same_token_fee: bool,
) -> Result<PublicBroadcasterAmountSplit> {
    public_broadcaster_amount_split_for_tokens_and_protocol(
        entered_amount,
        fee_amount,
        fee_mode,
        same_token_fee,
        U256::ZERO,
    )
}

pub(crate) fn public_broadcaster_amount_split_for_tokens_and_protocol(
    entered_amount: U256,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
    same_token_fee: bool,
    protocol_fee_bps: U256,
) -> Result<PublicBroadcasterAmountSplit> {
    if same_token_fee && protocol_fee_bps.is_zero() {
        return public_broadcaster_amount_split(entered_amount, fee_amount, fee_mode);
    }

    let receiver_amount = match fee_mode {
        FeeHandlingMode::DeductFromAmount => {
            if same_token_fee {
                if entered_amount <= fee_amount {
                    return Err(eyre!(
                        "entered amount must be greater than the broadcaster fee"
                    ));
                }
                entered_amount - fee_amount
            } else {
                entered_amount
            }
        }
        FeeHandlingMode::AddToAmount => {
            railgun_protocol_gross_amount_for_recipient(entered_amount, protocol_fee_bps)?
        }
    };
    let total_private_spend = if same_token_fee {
        receiver_amount + fee_amount
    } else {
        receiver_amount
    };

    Ok(PublicBroadcasterAmountSplit {
        entered_amount,
        receiver_amount,
        total_private_spend,
        fee_amount,
        fee_mode,
    })
}

pub(crate) fn public_broadcaster_max_entered_amount(
    max_receiver_amount: U256,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
) -> U256 {
    match fee_mode {
        FeeHandlingMode::DeductFromAmount => max_receiver_amount + fee_amount,
        FeeHandlingMode::AddToAmount => max_receiver_amount,
    }
}

#[cfg(test)]
pub(crate) fn public_broadcaster_max_entered_amount_for_tokens(
    max_receiver_amount: U256,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
    same_token_fee: bool,
) -> U256 {
    public_broadcaster_max_entered_amount_for_tokens_and_protocol(
        max_receiver_amount,
        fee_amount,
        fee_mode,
        same_token_fee,
        U256::ZERO,
    )
}

pub(crate) fn public_broadcaster_max_entered_amount_for_tokens_and_protocol(
    max_receiver_amount: U256,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
    same_token_fee: bool,
    protocol_fee_bps: U256,
) -> U256 {
    match fee_mode {
        FeeHandlingMode::DeductFromAmount => {
            if same_token_fee {
                public_broadcaster_max_entered_amount(max_receiver_amount, fee_amount, fee_mode)
            } else {
                max_receiver_amount
            }
        }
        FeeHandlingMode::AddToAmount => recipient_amount_after_protocol_fee(
            max_receiver_amount,
            railgun_protocol_fee_amount(max_receiver_amount, protocol_fee_bps),
        ),
    }
}

pub(crate) fn railgun_protocol_gross_amount_for_recipient(
    recipient_amount: U256,
    fee_bps: U256,
) -> Result<U256> {
    if recipient_amount.is_zero() || fee_bps.is_zero() {
        return Ok(recipient_amount);
    }
    if fee_bps >= FEE_BASIS_POINTS_DENOMINATOR {
        return Err(eyre!("RAILGUN protocol fee must be below 100%"));
    }

    let net_bps = FEE_BASIS_POINTS_DENOMINATOR - fee_bps;
    Ok(
        ((recipient_amount - U256::from(1)) * FEE_BASIS_POINTS_DENOMINATOR / net_bps)
            + U256::from(1),
    )
}

pub fn unshield_receiver_amount_for_fee_mode(
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
) -> Result<U256> {
    match fee_mode {
        FeeHandlingMode::DeductFromAmount => Ok(entered_amount),
        FeeHandlingMode::AddToAmount => {
            railgun_protocol_gross_amount_for_recipient(entered_amount, RAILGUN_PROTOCOL_FEE_BPS)
        }
    }
}

pub fn unshield_protocol_fee_amount_for_fee_mode(
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
) -> Result<U256> {
    let gross_amount = unshield_receiver_amount_for_fee_mode(entered_amount, fee_mode)?;
    Ok(match fee_mode {
        FeeHandlingMode::DeductFromAmount => {
            railgun_protocol_fee_amount(gross_amount, RAILGUN_PROTOCOL_FEE_BPS)
        }
        FeeHandlingMode::AddToAmount => gross_amount.saturating_sub(entered_amount),
    })
}

pub(super) const fn recipient_amount_after_protocol_fee(
    amount: U256,
    protocol_fee_amount: U256,
) -> U256 {
    amount.saturating_sub(protocol_fee_amount)
}

pub(crate) fn public_broadcaster_reported_amounts(
    action_token: Address,
    fee_token: Address,
    split: PublicBroadcasterAmountSplit,
    protocol_fee_bps: U256,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
) -> PublicBroadcasterReportedAmounts {
    if let Some(native_top_up) = native_top_up
        && action_token == native_top_up.wrapped_native_token
    {
        let combined_wrapped_native_amount = native_top_up_required_wrapped_native_amount(
            action_token,
            native_top_up.wrapped_native_token,
            split.receiver_amount,
            native_top_up.native_amount,
        );
        let protocol_fee_amount =
            railgun_protocol_fee_amount(combined_wrapped_native_amount, protocol_fee_bps);
        let recipient_amount = recipient_amount_after_protocol_fee(
            combined_wrapped_native_amount,
            protocol_fee_amount,
        )
        .saturating_sub(native_top_up.native_amount);
        let fee_spend = if action_token == fee_token {
            split.fee_amount
        } else {
            U256::ZERO
        };
        return PublicBroadcasterReportedAmounts {
            recipient_amount,
            total_private_spend: combined_wrapped_native_amount + fee_spend,
            protocol_fee_amount,
        };
    }

    let protocol_fee_amount = railgun_protocol_fee_amount(split.receiver_amount, protocol_fee_bps);
    PublicBroadcasterReportedAmounts {
        recipient_amount: recipient_amount_after_protocol_fee(
            split.receiver_amount,
            protocol_fee_amount,
        ),
        total_private_spend: split.total_private_spend,
        protocol_fee_amount,
    }
}

pub(crate) fn public_broadcaster_build_error(
    error: BuildError,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
    same_token_fee: bool,
    protocol_fee_bps: U256,
) -> Report {
    match error {
        BuildError::InsufficientBalance(max_receiver_amount) => eyre!(
            "{PUBLIC_BROADCASTER_MAX_ENTERED_AMOUNT_ERROR}{}",
            public_broadcaster_max_entered_amount_for_tokens_and_protocol(
                max_receiver_amount,
                fee_amount,
                fee_mode,
                same_token_fee,
                protocol_fee_bps,
            )
        ),
        BuildError::InsufficientFeeTokenBalance(max_spendable) => {
            eyre!(
                "{PUBLIC_BROADCASTER_FEE_TOKEN_MAX_SPENDABLE_ERROR}{max_spendable}{PUBLIC_BROADCASTER_REQUIRED_FEE_ERROR}{fee_amount}"
            )
        }
        other => Report::new(other),
    }
}

pub(super) struct PublicBroadcasterSetup {
    pub(super) chain: EffectiveDesktopChainConfig,
    pub(super) broadcaster: PublicBroadcasterCandidate,
    pub(super) query_rpc_pool: Arc<QueryRpcPool>,
    pub(super) min_gas_price: u128,
    pub(super) prover: ProverService,
    pub(super) forest: MerkleForest,
    pub(super) utxos: Vec<Utxo>,
}

#[derive(Clone)]
pub(super) struct EffectiveDesktopChainConfig {
    pub(super) rpc_urls: Vec<Url>,
    pub(super) railgun_contract: Address,
    pub(super) relay_adapt_contract: Address,
    pub(super) wrapped_native_token: Option<Address>,
    pub(super) finality_depth: u64,
    pub(super) gas: settings::EffectiveChainGasSettings,
}

pub(super) fn effective_desktop_chain_config(
    chain_id: u64,
    effective_chain: Option<&settings::EffectiveChainConfig>,
) -> Result<EffectiveDesktopChainConfig> {
    let defaults = chain_defaults_for_chain(chain_id)?;
    let Some(effective_chain) = effective_chain else {
        return Ok(EffectiveDesktopChainConfig {
            rpc_urls: defaults.rpc_urls,
            railgun_contract: defaults.contract,
            relay_adapt_contract: defaults.relay_adapt_contract,
            wrapped_native_token: wrapped_native_token_for_chain(chain_id),
            finality_depth: defaults.finality_depth,
            gas: settings::EffectiveChainGasSettings {
                gas_limit_buffer: GAS_LIMIT_BUFFER,
                gas_price_buffer_numerator: GAS_PRICE_BUFFER_NUMERATOR as u64,
                gas_price_buffer_denominator: GAS_PRICE_BUFFER_DENOMINATOR as u64,
            },
        });
    };
    if effective_chain.chain_id != chain_id {
        return Err(eyre!(
            "effective chain config is for chain {}, not {chain_id}",
            effective_chain.chain_id
        ));
    }
    let rpc_urls = parse_effective_rpc_urls(chain_id, &effective_chain.rpc_endpoints)?;
    let railgun_contract =
        parse_effective_address("railgun contract", &effective_chain.railgun_contract)?;
    let relay_adapt_contract = parse_effective_address(
        "relay adapt contract",
        &effective_chain.relay_adapt_contract,
    )?;
    let wrapped_native_token = effective_chain
        .wrapped_native_token
        .as_deref()
        .map(|value| parse_effective_address("wrapped native token", value))
        .transpose()?
        .or_else(|| wrapped_native_token_for_chain(chain_id));
    Ok(EffectiveDesktopChainConfig {
        rpc_urls,
        railgun_contract,
        relay_adapt_contract,
        wrapped_native_token,
        finality_depth: effective_chain.finality_depth,
        gas: effective_chain.gas.clone(),
    })
}

pub(crate) fn parse_effective_rpc_urls(
    chain_id: u64,
    rpc_endpoints: &[String],
) -> Result<Vec<Url>> {
    if rpc_endpoints.is_empty() {
        return Err(eyre!("effective chain {chain_id} has no RPC endpoints"));
    }
    rpc_endpoints
        .iter()
        .map(|url| Url::parse(url).wrap_err_with(|| format!("parse RPC URL {url}")))
        .collect()
}

pub(crate) fn effective_rpc_urls_for_chain(
    defaults: &ChainConfigDefaults,
    effective_chain: Option<&settings::EffectiveChainConfig>,
) -> Result<Vec<Url>> {
    effective_chain.map_or_else(
        || Ok(defaults.rpc_urls.clone()),
        |chain| parse_effective_rpc_urls(defaults.chain_id, &chain.rpc_endpoints),
    )
}

pub(crate) fn query_rpc_pool_with_http_client(
    rpc_urls: Vec<Url>,
    http: &HttpContext,
) -> Arc<QueryRpcPool> {
    Arc::new(QueryRpcPool::with_http_client(
        rpc_urls,
        DEFAULT_QUERY_RPC_COOLDOWN,
        http.rpc_client.clone(),
    ))
}

pub(crate) async fn buffered_gas_price_from_rpc_pool(
    query_rpc_pool: &QueryRpcPool,
    gas: &settings::EffectiveChainGasSettings,
) -> Result<u128> {
    let mut last_error = None;
    for _ in 0..query_rpc_pool.len() {
        let Some(provider_handle) = query_rpc_pool.random_provider() else {
            break;
        };
        match buffered_gas_price_with_policy(
            &provider_handle.provider,
            u128::from(gas.gas_price_buffer_numerator),
            u128::from(gas.gas_price_buffer_denominator),
        )
        .await
        {
            Ok(gas_price) => return Ok(gas_price),
            Err(error) => {
                let rpc = http::redact_url_for_display(&provider_handle.url);
                tracing::warn!(%error, %rpc, "fetch gas price failed");
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(error);
            }
        }
    }
    if let Some(error) = last_error {
        Err(error).wrap_err("all query RPC gas price attempts failed")
    } else {
        Err(eyre!("no healthy query RPC available"))
    }
}

pub(super) fn is_effective_wrapped_native_token(
    chain_id: u64,
    token: Address,
    chain: &EffectiveDesktopChainConfig,
) -> bool {
    chain.wrapped_native_token.map_or_else(
        || is_wrapped_native_token(chain_id, token),
        |wrapped| wrapped == token,
    )
}

pub(crate) fn public_broadcaster_anchor_rate_for_policy(
    anchor_cache: Option<&Arc<TokenAnchorRateCache>>,
    chain_id: u64,
    token: Address,
) -> Option<U256> {
    anchor_cache
        .and_then(|cache| cache.cached_rate(chain_id, token))
        .or_else(|| fixed_token_anchor_rate(chain_id, token))
}

pub(super) async fn public_broadcaster_setup(
    session: &WalletSession,
    chain_id: u64,
    effective_chain: Option<&settings::EffectiveChainConfig>,
    token: Address,
    fee_rows: &[FeeRow],
    selection: &PublicBroadcasterSelection,
    require_relay_adapt: bool,
    policy: BroadcasterFeePolicy,
    trust_filter: &PublicBroadcasterTrustFilter,
    anchor_cache: Option<&Arc<TokenAnchorRateCache>>,
    http: &HttpContext,
) -> Result<PublicBroadcasterSetup> {
    let chain = effective_desktop_chain_config(chain_id, effective_chain)?;
    let anchor_rate = public_broadcaster_anchor_rate_for_policy(anchor_cache, chain_id, token);
    let candidates = public_broadcaster_candidates(
        fee_rows,
        chain_id,
        token,
        if require_relay_adapt {
            Some(chain.relay_adapt_contract)
        } else {
            None
        },
        SystemTime::now(),
        policy,
        anchor_rate,
    );
    let broadcaster = select_public_broadcaster_with_policy_and_trust(
        &candidates,
        selection,
        policy,
        trust_filter,
    )?;
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls.clone(), http);
    let min_gas_price = buffered_gas_price_from_rpc_pool(&query_rpc_pool, &chain.gas).await?;
    let artifact_source = artifact_source(http, session.db.as_ref())?;
    let prover = ProverService::new_with_db(&artifact_source, &session.db);
    let chain_handle = session
        .sync_manager
        .chain_handle(&session.chain_key)
        .await
        .ok_or_else(|| eyre!("chain handle not found for chain {chain_id}"))?;
    let mut forest = chain_handle.forest.read().await.clone();
    forest.compute_roots();
    let utxos = session.unspent_utxos();

    Ok(PublicBroadcasterSetup {
        chain,
        broadcaster,
        query_rpc_pool,
        min_gas_price,
        prover,
        forest,
        utxos,
    })
}

pub(crate) const fn approximate_public_broadcaster_gas(shape: ApproximateTransactionShape) -> u64 {
    let raw = APPROX_BASE_GAS
        + APPROX_GAS_PER_TRANSACTION * shape.transaction_count.saturating_sub(1) as u64
        + APPROX_GAS_PER_INPUT * shape.input_count as u64
        + APPROX_GAS_PER_PRIVATE_OUTPUT * shape.private_output_count as u64
        + APPROX_GAS_PER_PUBLIC_OUTPUT * shape.public_output_count as u64
        + if shape.send { APPROX_SEND_EXTRA_GAS } else { 0 }
        + APPROX_UNWRAP_EXTRA_GAS * shape.unwrap_count as u64
        + APPROX_SAFETY_GAS;
    raw.saturating_mul(APPROX_GAS_UPLIFT_NUMERATOR)
        .saturating_add(APPROX_GAS_UPLIFT_DENOMINATOR - 1)
        / APPROX_GAS_UPLIFT_DENOMINATOR
}

pub(super) const fn gas_shortfall_bps(
    predicted_gas_limit: u64,
    actual_gas_limit: u64,
) -> Option<u64> {
    if predicted_gas_limit == 0 || actual_gas_limit <= predicted_gas_limit {
        return None;
    }
    Some((actual_gas_limit - predicted_gas_limit) * 10_000 / predicted_gas_limit)
}

pub(super) fn log_public_broadcaster_fee_prediction_failure(
    action: &'static str,
    attempt: usize,
    available_fee: U256,
    computed_fee: U256,
    gas_limit: u64,
    estimate: Option<&PublicBroadcasterCostEstimate>,
    plan_transaction_count: usize,
    plan_input_count: usize,
    plan_private_output_count: usize,
    plan_public_output_count: usize,
    broadcaster: &PublicBroadcasterCandidate,
) {
    let predicted_gas_limit = estimate.map(|estimate| estimate.gas_limit);
    let gas_shortfall = predicted_gas_limit.map(|predicted| gas_limit.saturating_sub(predicted));
    let gas_shortfall_bps =
        predicted_gas_limit.and_then(|predicted| gas_shortfall_bps(predicted, gas_limit));
    let estimated_transaction_count = estimate.map(|estimate| estimate.transaction_count);
    let estimated_input_count = estimate.map(|estimate| estimate.input_count);
    let estimated_private_output_count = estimate.map(|estimate| estimate.private_output_count);
    let estimated_public_output_count = estimate.map(|estimate| estimate.public_output_count);
    tracing::warn!(
        action,
        attempt,
        available_fee = %available_fee,
        computed_fee = %computed_fee,
        fee_shortfall = %computed_fee.saturating_sub(available_fee),
        gas_limit,
        ?predicted_gas_limit,
        ?gas_shortfall,
        ?gas_shortfall_bps,
        plan_transaction_count,
        plan_input_count,
        plan_private_output_count,
        plan_public_output_count,
        ?estimated_transaction_count,
        ?estimated_input_count,
        ?estimated_private_output_count,
        ?estimated_public_output_count,
        broadcaster = %broadcaster.railgun_address,
        fees_id = %broadcaster.fees_id,
        "public broadcaster fee prediction failed; retrying with buffered fee"
    );
}

pub(crate) fn approximate_public_broadcaster_cost(
    broadcaster: PublicBroadcasterCandidate,
    action_token: Address,
    fee_token: Address,
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
    protocol_fee_bps: U256,
    min_gas_price: u128,
    initial_fee_amount: U256,
    mut select_shape: impl FnMut(PublicBroadcasterAmountSplit) -> Result<ApproximateTransactionShape>,
) -> Result<PublicBroadcasterCostEstimate> {
    let service_gas_price = public_broadcaster_service_gas_price(min_gas_price);
    let mut fee_amount = initial_fee_amount;
    let mut latest_shape = None;
    let mut latest_split = None;
    let mut latest_gas_limit = 0;
    let same_token_fee = action_token == fee_token;

    for _ in 0..PUBLIC_BROADCASTER_FEE_ATTEMPTS {
        let split = public_broadcaster_amount_split_for_tokens_and_protocol(
            entered_amount,
            fee_amount,
            fee_mode,
            same_token_fee,
            protocol_fee_bps,
        )?;
        let shape = select_shape(split)?;
        let gas_limit = approximate_public_broadcaster_gas(shape);
        let computed_fee = broadcaster_fee_amount(broadcaster.fee, gas_limit, service_gas_price);
        latest_shape = Some(shape);
        latest_split = Some(split);
        latest_gas_limit = gas_limit;
        if broadcaster_fee_covers(fee_amount, computed_fee) {
            let protocol_fee_amount =
                railgun_protocol_fee_amount(split.receiver_amount, protocol_fee_bps);
            return Ok(PublicBroadcasterCostEstimate {
                broadcaster,
                action_token,
                fee_token,
                entered_amount: split.entered_amount,
                receiver_amount: split.receiver_amount,
                recipient_amount: recipient_amount_after_protocol_fee(
                    split.receiver_amount,
                    protocol_fee_amount,
                ),
                total_private_spend: split.total_private_spend,
                fee_amount,
                protocol_fee_amount,
                protocol_fee_bps,
                fee_mode: split.fee_mode,
                max_receiver_amount: shape.max_receiver_amount,
                max_entered_amount: public_broadcaster_max_entered_amount_for_tokens_and_protocol(
                    shape.max_receiver_amount,
                    fee_amount,
                    split.fee_mode,
                    same_token_fee,
                    protocol_fee_bps,
                ),
                gas_limit,
                min_gas_price,
                native_gas_cost: public_broadcaster_native_gas_cost(gas_limit, min_gas_price),
                transaction_count: shape.transaction_count,
                input_count: shape.input_count,
                private_output_count: shape.private_output_count,
                public_output_count: shape.public_output_count,
                relay_call_count: shape.relay_call_count,
                uses_relay_adapt: shape.uses_relay_adapt,
                native_top_up: None,
            });
        }
        fee_amount = buffered_public_broadcaster_fee(computed_fee);
    }

    let shape = latest_shape.ok_or_else(|| eyre!("could not estimate public broadcaster cost"))?;
    let split = latest_split.ok_or_else(|| eyre!("could not estimate public broadcaster cost"))?;
    let protocol_fee_amount = railgun_protocol_fee_amount(split.receiver_amount, protocol_fee_bps);
    Ok(PublicBroadcasterCostEstimate {
        broadcaster,
        action_token,
        fee_token,
        entered_amount: split.entered_amount,
        receiver_amount: split.receiver_amount,
        recipient_amount: recipient_amount_after_protocol_fee(
            split.receiver_amount,
            protocol_fee_amount,
        ),
        total_private_spend: split.total_private_spend,
        fee_amount: split.fee_amount,
        protocol_fee_amount,
        protocol_fee_bps,
        fee_mode: split.fee_mode,
        max_receiver_amount: shape.max_receiver_amount,
        max_entered_amount: public_broadcaster_max_entered_amount_for_tokens_and_protocol(
            shape.max_receiver_amount,
            split.fee_amount,
            split.fee_mode,
            same_token_fee,
            protocol_fee_bps,
        ),
        gas_limit: latest_gas_limit,
        min_gas_price,
        native_gas_cost: public_broadcaster_native_gas_cost(latest_gas_limit, min_gas_price),
        transaction_count: shape.transaction_count,
        input_count: shape.input_count,
        private_output_count: shape.private_output_count,
        public_output_count: shape.public_output_count,
        relay_call_count: shape.relay_call_count,
        uses_relay_adapt: shape.uses_relay_adapt,
        native_top_up: None,
    })
}

pub(crate) fn initial_separate_token_public_broadcaster_fee(
    broadcaster: &PublicBroadcasterCandidate,
    min_gas_price: u128,
    seed_shape: ApproximateTransactionShape,
) -> U256 {
    let service_gas_price = public_broadcaster_service_gas_price(min_gas_price);
    let gas_limit = approximate_public_broadcaster_gas(seed_shape);
    buffered_public_broadcaster_fee(broadcaster_fee_amount(
        broadcaster.fee,
        gas_limit,
        service_gas_price,
    ))
}

pub(super) fn initial_public_broadcaster_fee_amount(
    broadcaster: &PublicBroadcasterCandidate,
    min_gas_price: u128,
    same_token_fee: bool,
    seed_shape: impl FnOnce() -> Result<ApproximateTransactionShape>,
) -> Result<U256> {
    if same_token_fee {
        Ok(U256::ZERO)
    } else {
        Ok(initial_separate_token_public_broadcaster_fee(
            broadcaster,
            min_gas_price,
            seed_shape()?,
        ))
    }
}

pub(crate) const fn send_approximate_shape(
    selection: &railgun_wallet::tx::UnshieldSelectionInfo,
    max_receiver_amount: U256,
) -> ApproximateTransactionShape {
    ApproximateTransactionShape {
        transaction_count: selection.transaction_count,
        input_count: selection.input_count,
        private_output_count: selection.private_output_count,
        public_output_count: 0,
        max_receiver_amount,
        relay_call_count: 0,
        uses_relay_adapt: false,
        unwrap_count: 0,
        send: true,
    }
}

pub(crate) const fn unshield_approximate_shape(
    selection: &railgun_wallet::tx::UnshieldSelectionInfo,
    max_receiver_amount: U256,
    unwrap: bool,
) -> ApproximateTransactionShape {
    ApproximateTransactionShape {
        transaction_count: selection.transaction_count,
        input_count: selection.input_count,
        private_output_count: selection.private_output_count,
        public_output_count: selection.public_output_count,
        max_receiver_amount,
        relay_call_count: if unwrap { 1 } else { 0 },
        uses_relay_adapt: unwrap,
        unwrap_count: if unwrap { 1 } else { 0 },
        send: false,
    }
}

pub(crate) fn native_top_up_approximate_shape(
    utxos: &[Utxo],
    token: Address,
    fee_token: Address,
    receiver_amount: U256,
    fee_amount: U256,
    native_top_up: &DesktopNativeTopUpPlan,
) -> Result<ApproximateTransactionShape> {
    let wrapped_native = native_top_up.wrapped_native_token;
    if token == wrapped_native {
        let combined_wrapped_native_amount = native_top_up_required_wrapped_native_amount(
            token,
            wrapped_native,
            receiver_amount,
            native_top_up.native_amount,
        );
        let selection = native_top_up_selection_info_with_broadcaster_fee_seed(
            utxos,
            wrapped_native,
            fee_token,
            combined_wrapped_native_amount,
            fee_amount,
        )?;
        let max_combined_net = native_top_up_net_after_protocol_fee(selection.max_spendable);
        let max_receiver_amount = native_top_up_wrapped_native_amount_for_net(
            max_combined_net.saturating_sub(native_top_up.native_amount),
        );
        return Ok(ApproximateTransactionShape {
            transaction_count: selection.transaction_count,
            input_count: selection.input_count,
            private_output_count: selection.private_output_count,
            public_output_count: selection.public_output_count,
            max_receiver_amount,
            relay_call_count: 3,
            uses_relay_adapt: true,
            unwrap_count: 1,
            send: false,
        });
    }

    let primary_selection = if fee_token == wrapped_native {
        unshield_selection_info(utxos, token, receiver_amount, false)?
    } else {
        native_top_up_selection_info_with_broadcaster_fee_seed(
            utxos,
            token,
            fee_token,
            receiver_amount,
            fee_amount,
        )?
    };
    let top_up_selection = if fee_token == wrapped_native {
        unshield_selection_info_with_broadcaster_fee_token(
            utxos,
            wrapped_native,
            wrapped_native,
            native_top_up.wrapped_native_amount,
            fee_amount,
            false,
        )?
    } else {
        unshield_selection_info(
            utxos,
            wrapped_native,
            native_top_up.wrapped_native_amount,
            false,
        )?
    };
    let transaction_count =
        primary_selection.transaction_count + top_up_selection.transaction_count;
    if transaction_count > MAX_BATCH_TRANSACTIONS {
        return Err(Report::new(BuildError::TooManyBatchTransactions {
            requested: transaction_count,
            max: MAX_BATCH_TRANSACTIONS,
        }));
    }

    Ok(ApproximateTransactionShape {
        transaction_count,
        input_count: primary_selection.input_count + top_up_selection.input_count,
        private_output_count: primary_selection.private_output_count
            + top_up_selection.private_output_count,
        public_output_count: primary_selection.public_output_count
            + top_up_selection.public_output_count,
        max_receiver_amount: primary_selection.max_spendable,
        relay_call_count: 2,
        uses_relay_adapt: true,
        unwrap_count: 1,
        send: false,
    })
}

fn native_top_up_selection_info_with_broadcaster_fee_seed(
    utxos: &[Utxo],
    token: Address,
    fee_token: Address,
    amount: U256,
    fee_amount: U256,
) -> Result<railgun_wallet::tx::UnshieldSelectionInfo> {
    if fee_amount.is_zero() && token != fee_token {
        return unshield_selection_info_with_separate_broadcaster_fee_seed(
            utxos, token, fee_token, amount, false,
        )
        .map_err(Report::new);
    }

    unshield_selection_info_with_broadcaster_fee_token(
        utxos, token, fee_token, amount, fee_amount, false,
    )
    .map_err(Report::new)
}

#[must_use]
pub fn transact_topic(chain_id: u64) -> String {
    ContentTopic::transact_topic(chain_id)
}

#[must_use]
pub fn transact_response_topic(chain_id: u64) -> String {
    ContentTopic::transact_response_topic(chain_id)
}

pub fn decode_public_broadcaster_response(
    shared_key: &[u8; 32],
    payload: &[u8],
) -> Result<Option<PublicBroadcasterResultKind>> {
    Ok(
        match DecryptedTransactResponse::try_decrypt_message(shared_key, payload)? {
            Some(DecryptedTransactResponse::TxHash(tx_hash)) => {
                Some(PublicBroadcasterResultKind::Submitted { tx_hash })
            }
            Some(DecryptedTransactResponse::Error(error)) => {
                Some(PublicBroadcasterResultKind::Failed { error })
            }
            None => None,
        },
    )
}

pub(super) fn supported_broadcaster_version(version: &str) -> bool {
    version
        .split('.')
        .next()
        .and_then(|major| major.parse::<u64>().ok())
        == Some(8)
}
