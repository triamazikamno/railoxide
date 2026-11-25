use super::*;
use alloy::primitives::TxHash;
use alloy::uint;
use eyre::eyre;
use railgun_wallet::tx::{GasEstimateMode, RailgunGasModel, TransactGasShape};
use sha2::{Digest, Sha256};

mod amount_split;
mod approximate_shapes;
mod cost_approximation;
mod fee_amounts;
mod request_setup;
mod selection;

pub(crate) use amount_split::*;
pub(crate) use approximate_shapes::*;
pub(crate) use cost_approximation::*;
pub use fee_amounts::*;
pub(crate) use request_setup::*;
pub use selection::*;

/// Keeps the tracing target of the moved items at this module's path.
const LOG_TARGET: &str = module_path!();

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
    pub relay_adapt: Option<Address>,
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

/// The notes of an earlier broadcaster proof for this payment, which may still execute. An
/// attempt that spends one of them can't execute alongside it, so it can't pay twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicBroadcasterInputPin {
    pub inputs: Vec<SelectedInputIdentity>,
    /// The user accepted that the earlier proof may still execute, so selection is free.
    pub overridden: bool,
}

impl PublicBroadcasterInputPin {
    /// The notes an attempt must share at least one of, unless the pin is overridden.
    pub(crate) fn active(pin: Option<&Self>) -> Option<&[SelectedInputIdentity]> {
        pin.filter(|pin| !pin.overridden)
            .map(|pin| pin.inputs.as_slice())
    }
}

/// A pinned attempt that isn't one payload transaction sharing a pinned note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "The notes of this payment's earlier proof can't cover this attempt. Submit again to choose whether to use other notes."
)]
pub struct PublicBroadcasterInputPinUnavailable;

pub struct DesktopUnshieldPublicBroadcasterRequest {
    /// Exact total payment in fee-token base units; `None` uses automatic estimation.
    pub custom_fee_amount: Option<U256>,
    /// Notes an earlier, possibly executable proof of this form spent.
    pub input_pin: Option<PublicBroadcasterInputPin>,
    pub executor: Option<Arc<PreparedExecutorOperation>>,
    /// The fee ceiling the user approved; required for automatic fees and for executor operations.
    pub maximum_private_fee: Option<U256>,
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
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
    /// The origin of the gateway draft this submits, which its journal entry is tagged with.
    pub gateway_origin: Option<String>,
    pub verify_proof: bool,
    /// The current fee estimate within the approved bounds; submission pays its broadcaster's quoted rate.
    pub quote: PublicBroadcasterCostEstimate,
    pub fee_mode: FeeHandlingMode,
    pub fee_policy: BroadcasterFeePolicy,
    pub trust_filter: PublicBroadcasterTrustFilter,
    pub anchor_cache: Option<Arc<TokenAnchorRateCache>>,
    pub waku: PublicBroadcasterWaku,
    pub response_timeout: Duration,
    pub republish_interval: Duration,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
    pub route_tx: Option<SubmissionRouteSender>,
    /// Receives the final single-payload notes before publication starts.
    pub payload_inputs_tx: Option<tokio::sync::oneshot::Sender<Vec<SelectedInputIdentity>>>,
}

pub struct DesktopSendPublicBroadcasterRequest {
    /// Exact total payment in fee-token base units; `None` uses automatic estimation.
    pub custom_fee_amount: Option<U256>,
    /// Notes an earlier, possibly executable proof of this form spent.
    pub input_pin: Option<PublicBroadcasterInputPin>,
    /// The fee ceiling the user approved; required for automatic fees.
    pub maximum_private_fee: Option<U256>,
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
    /// The origin of the gateway draft this submits, which its journal entry is tagged with.
    pub gateway_origin: Option<String>,
    pub verify_proof: bool,
    /// The current fee estimate within the approved bounds; submission pays its broadcaster's quoted rate.
    pub quote: PublicBroadcasterCostEstimate,
    pub fee_mode: FeeHandlingMode,
    pub fee_policy: BroadcasterFeePolicy,
    pub trust_filter: PublicBroadcasterTrustFilter,
    pub anchor_cache: Option<Arc<TokenAnchorRateCache>>,
    pub waku: PublicBroadcasterWaku,
    pub response_timeout: Duration,
    pub republish_interval: Duration,
    pub progress_tx: Option<TransactionGenerationProgressSender>,
    pub route_tx: Option<SubmissionRouteSender>,
    /// Receives the final single-payload notes before publication starts.
    pub payload_inputs_tx: Option<tokio::sync::oneshot::Sender<Vec<SelectedInputIdentity>>>,
}

pub struct DesktopUnshieldPublicBroadcasterEstimateRequest {
    pub custom_fee_amount: Option<U256>,
    pub input_pin: Option<PublicBroadcasterInputPin>,
    pub executor: Option<Arc<PreparedExecutorOperation>>,
    /// Without `executor`, the operation a later submission reuses: its own reserved notes
    /// stay admissible.
    pub reused_executor_operation: Option<vault::ExecutorOperationId>,
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
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
    pub custom_fee_amount: Option<U256>,
    pub input_pin: Option<PublicBroadcasterInputPin>,
    pub chain_id: u64,
    pub effective_chain: settings::EffectiveChainConfig,
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
    /// The unshields of the estimate's final selection, with their notes resolved, for the
    /// amount privacy hints. `None` for a send, a native top-up, or notes that didn't resolve.
    pub planned_unshields: Option<Vec<crate::ResolvedPlannedUnshield>>,
}

/// Reviewed spending limits, separate from the fee placed in the transaction.
#[derive(Clone, Copy)]
pub struct PublicBroadcasterApprovalBounds {
    maximum_fee: U256,
    minimum_recipient_amount: U256,
    maximum_private_spend: U256,
}

impl PublicBroadcasterApprovalBounds {
    #[must_use]
    pub const fn maximum_fee(self) -> U256 {
        self.maximum_fee
    }

    #[must_use]
    pub const fn minimum_recipient_amount(self) -> U256 {
        self.minimum_recipient_amount
    }

    #[must_use]
    pub fn covers(self, quote: &PublicBroadcasterCostEstimate) -> bool {
        quote.fee_amount <= self.maximum_fee
            && quote.recipient_amount >= self.minimum_recipient_amount
            && quote.total_private_spend <= self.maximum_private_spend
    }
}

impl PublicBroadcasterCostEstimate {
    /// Calculate approval limits without changing this quote or its payment amount.
    pub fn approval_bounds(&self, maximum_fee: U256) -> Result<PublicBroadcasterApprovalBounds> {
        let same_token_fee = self.action_token == self.fee_token;
        // Deducted fees must leave a positive unshield amount.
        let maximum_fee = if same_token_fee && self.fee_mode == FeeHandlingMode::DeductFromAmount {
            maximum_fee.min(self.entered_amount.saturating_sub(U256::ONE))
        } else {
            maximum_fee
        };
        let split = public_broadcaster_amount_split_for_tokens_and_protocol(
            self.entered_amount,
            maximum_fee,
            self.fee_mode,
            same_token_fee,
            self.protocol_fee_bps,
        )?;
        let amounts = public_broadcaster_reported_amounts(
            self.action_token,
            self.fee_token,
            split,
            self.protocol_fee_bps,
            self.native_top_up.as_ref(),
        );
        Ok(PublicBroadcasterApprovalBounds {
            maximum_fee,
            minimum_recipient_amount: amounts.recipient_amount,
            maximum_private_spend: amounts.total_private_spend,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicBroadcasterResultKind {
    Submitted { tx_hash: TxHash },
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
    /// The notes of the attempt's only payload transaction; `None` for a payload of several
    /// transactions or an executor operation.
    pub payload_inputs: Option<Vec<SelectedInputIdentity>>,
}

#[derive(Debug, Clone)]
pub(super) struct PreparedPublicBroadcasterPlan<P> {
    pub(super) transaction: Option<TransactionRequest>,
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

impl<P> PreparedPublicBroadcasterPlan<P> {
    pub(super) fn into_submission_result(
        self,
        result: PublicBroadcasterResultKind,
        payload_inputs: Option<Vec<SelectedInputIdentity>>,
    ) -> PublicBroadcasterSubmissionResult {
        let Self {
            transaction: _,
            plan: _,
            pre_transaction_pois_per_txid_leaf_per_list: _,
            broadcaster,
            action_token,
            fee_token,
            entered_amount,
            receiver_amount,
            recipient_amount,
            total_private_spend,
            fee_amount,
            protocol_fee_amount,
            protocol_fee_bps,
            fee_mode,
            gas_limit,
            min_gas_price,
            bound_min_gas_price: _,
            transaction_count,
            input_count,
            private_output_count,
            public_output_count,
            relay_call_count,
            uses_relay_adapt,
            native_top_up,
        } = self;
        PublicBroadcasterSubmissionResult {
            broadcaster,
            action_token,
            fee_token,
            entered_amount,
            receiver_amount,
            recipient_amount,
            total_private_spend,
            fee_amount,
            protocol_fee_amount,
            protocol_fee_bps,
            fee_mode,
            gas_limit,
            min_gas_price,
            transaction_count,
            input_count,
            private_output_count,
            public_output_count,
            relay_call_count,
            uses_relay_adapt,
            result,
            native_top_up,
            payload_inputs,
        }
    }
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
    pub(crate) executor: bool,
}

impl ApproximateTransactionShape {
    pub(crate) const fn with_executor(mut self, executor: bool) -> Self {
        self.executor = executor;
        if executor && self.relay_call_count == 1 {
            // The executor unwrap uses separate exact unwrap and transfer calls.
            self.relay_call_count = 2;
        }
        self
    }
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

pub(super) struct PublicBroadcasterSetup {
    pub(super) chain: EffectiveDesktopChainConfig,
    pub(super) broadcaster: PublicBroadcasterCandidate,
    pub(super) query_rpc_pool: Arc<QueryRpcPool>,
    pub(super) min_gas_price: u128,
    pub(super) prover: ProverService,
    pub(super) forest: MerkleForest,
    pub(super) utxos: Vec<Utxo>,
}

/// Basis points of a transaction's `UpperBound` execution gas that an approved Arbitrum One gas
/// ceiling reserves for L1 data gas.
///
/// The shared gas model excludes L1 data gas, but Arbitrum's `eth_estimateGas` includes it.
/// Samples from 2026-09-28 (325 Railgun calls, blocks 508,687,391-509,677,298) put
/// `gasUsedForL1` at a median of 2.5k, a p99 of 116k and a max of 426k, up to 26.1% of L2 gas.
/// This is headroom sized from history, not a model of L1 pricing. A spike beyond it still fails
/// the approved-maximum check safely, and the user can retry. A complete fix would size data gas
/// from the encoded calldata and a live `ArbGasInfo` L1 price snapshot taken at quote time.
pub(crate) const ARBITRUM_ONE_DATA_GAS_ALLOWANCE_BPS: u64 = 3_500;

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
