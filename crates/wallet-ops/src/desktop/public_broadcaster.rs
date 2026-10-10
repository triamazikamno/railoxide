use super::*;
use alloy::uint;
use eyre::eyre;
use railgun_wallet::tx::{GasEstimateMode, RailgunGasModel, TransactGasShape};
use sha2::{Digest, Sha256};

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

pub struct DesktopUnshieldPublicBroadcasterRequest {
    /// Exact total payment in fee-token base units; `None` uses automatic estimation.
    pub custom_fee_amount: Option<U256>,
    pub executor: Option<Arc<PreparedExecutorOperation>>,
    pub executor_maximum_private_fee: Option<U256>,
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
    /// Exact total payment in fee-token base units; `None` uses automatic estimation.
    pub custom_fee_amount: Option<U256>,
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
    pub custom_fee_amount: Option<U256>,
    pub executor: Option<Arc<PreparedExecutorOperation>>,
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

pub fn eligible_public_broadcasters_for_asset(
    rows: &[FeeRow],
    chain_id: u64,
    token: Address,
    required_relay_adapt: Option<Address>,
) -> Result<Vec<PublicBroadcasterCandidate>> {
    broadcaster_core::deployment::RailgunDeployment::for_chain(chain_id)
        .ok_or_else(|| eyre!("chain {chain_id} has no Railgun deployment"))?;
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
    broadcaster_core::deployment::RailgunDeployment::for_chain(chain_id)
        .ok_or_else(|| eyre!("chain {chain_id} has no Railgun deployment"))?;
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
    min_gas_price * 125 / 100
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

/// Allow a 25% increase over the estimated fee in spend review. This is an approval
/// ceiling; payment still uses the required fee with the ordinary fee buffer.
#[must_use]
pub fn default_public_broadcaster_fee_limit(estimated_fee: U256) -> U256 {
    estimated_fee.saturating_add(estimated_fee / U256::from(4))
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

pub(crate) fn bounded_public_broadcaster_fee(
    required_fee: U256,
    maximum: Option<U256>,
) -> Result<U256> {
    if maximum.is_some_and(|maximum| required_fee > maximum) {
        return Err(eyre!(
            "executor broadcaster fee exceeds the reviewed maximum; refresh and approve the quote"
        ));
    }
    let buffered = buffered_public_broadcaster_fee(required_fee);
    Ok(maximum.map_or(buffered, |maximum| buffered.min(maximum)))
}

pub(crate) fn validate_custom_public_broadcaster_fee(
    amount: U256,
    required: U256,
    maximum: Option<U256>,
) -> Result<U256> {
    if amount.is_zero() || amount < required {
        return Err(eyre!(
            "Custom transaction fee is too low. Increase it or use automatic estimation."
        ));
    }
    if maximum.is_some_and(|maximum| amount > maximum) {
        return Err(eyre!(
            "Custom transaction fee exceeds the reviewed maximum. Review the updated fee before submitting."
        ));
    }
    Ok(amount)
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
        FeeHandlingMode::AddToAmount => (
            entered_amount,
            entered_amount
                .checked_add(fee_amount)
                .ok_or_else(|| eyre!("amount plus transaction fee exceeds the supported range"))?,
        ),
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
        receiver_amount
            .checked_add(fee_amount)
            .ok_or_else(|| eyre!("amount plus transaction fee exceeds the supported range"))?
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

pub(crate) fn public_broadcaster_anchor_rate_for_policy(
    anchor_cache: Option<&Arc<TokenAnchorRateCache>>,
    chain_id: u64,
    token: Address,
) -> Option<U256> {
    anchor_cache
        .and_then(|cache| cache.cached_rate(chain_id, token))
        .or_else(|| fixed_token_anchor_rate(chain_id, token))
}

/// Executor requests keep the quote selected before approval until it expires.
/// Refreshed advertisements are candidates for a new preparation, not this one.
pub(crate) fn public_broadcaster_for_request(
    fee_rows: &[FeeRow],
    chain_id: u64,
    token: Address,
    required_relay_adapt: Option<Address>,
    selection: &PublicBroadcasterSelection,
    policy: BroadcasterFeePolicy,
    trust_filter: &PublicBroadcasterTrustFilter,
    anchor_rate: Option<U256>,
    executor_delivery: Option<&ExecutorDelivery>,
) -> Result<PublicBroadcasterCandidate> {
    let candidates = if let Some(delivery) = executor_delivery {
        let ExecutorDelivery::PublicBroadcaster(candidate) = delivery else {
            return Err(eyre!("executor preparation selected another funding route"));
        };
        if candidate.chain_id != chain_id || candidate.token != token {
            return Err(eyre!("executor broadcaster chain or fee token changed"));
        }
        let profile = candidate
            .relay_adapt_7702
            .and_then(|delegate| settings::ExecutorProfile::accepted(chain_id, delegate))
            .ok_or_else(|| eyre!("broadcaster executor profile is unavailable"))?;
        delivery.admit(profile)?;
        let mut candidate = candidate.as_ref().clone();
        candidate.fee_policy_status = policy.classify_fee(candidate.fee, anchor_rate);
        vec![candidate]
    } else {
        public_broadcaster_candidates(
            fee_rows,
            chain_id,
            token,
            required_relay_adapt,
            SystemTime::now(),
            policy,
            anchor_rate,
        )
    };
    select_public_broadcaster_with_policy_and_trust(&candidates, selection, policy, trust_filter)
}

pub(super) async fn public_broadcaster_setup(
    session: &WalletSession,
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    token: Address,
    fee_rows: &[FeeRow],
    selection: &PublicBroadcasterSelection,
    require_relay_adapt: bool,
    executor_delivery: Option<&ExecutorDelivery>,
    policy: BroadcasterFeePolicy,
    trust_filter: &PublicBroadcasterTrustFilter,
    anchor_cache: Option<&Arc<TokenAnchorRateCache>>,
    http: &HttpContext,
) -> Result<PublicBroadcasterSetup> {
    let chain = effective_desktop_chain_config(chain_id, effective_chain)?;
    let anchor_rate = public_broadcaster_anchor_rate_for_policy(anchor_cache, chain_id, token);
    let broadcaster = public_broadcaster_for_request(
        fee_rows,
        chain_id,
        token,
        if require_relay_adapt {
            Some(chain.relay_adapt_contract)
        } else {
            None
        },
        selection,
        policy,
        trust_filter,
        anchor_rate,
        executor_delivery,
    )?;
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls.clone(), http);
    // Refresh after approval, then keep this price stable while building the proof.
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

/// L1 data gas that an approved gas ceiling adds to `execution_gas`. Zero outside Arbitrum One.
pub(crate) const fn arbitrum_data_gas_allowance(chain_id: u64, execution_gas: u64) -> u64 {
    if chain_id == 42161 {
        execution_gas.saturating_mul(ARBITRUM_ONE_DATA_GAS_ALLOWANCE_BPS) / 10_000
    } else {
        0
    }
}

/// Gas of a transaction with `shape` in `mode`, including the intrinsic gas and excluding the
/// chain's gas limit buffer. Each public output is one transaction's unshield. Final proved
/// requests use RPC estimation.
pub(crate) const fn approximate_public_broadcaster_gas(
    model: &RailgunGasModel,
    mode: GasEstimateMode,
    shape: ApproximateTransactionShape,
) -> u64 {
    let transact = model.transact(
        mode,
        TransactGasShape {
            transactions: shape.transaction_count,
            inputs: shape.input_count,
            outputs: shape
                .private_output_count
                .saturating_add(shape.public_output_count),
            unshields: shape.public_output_count,
        },
    );
    let relay = if shape.uses_relay_adapt {
        model.relay(shape.relay_call_count)
    } else {
        0
    };
    let executor = if shape.executor { model.executor() } else { 0 };
    TRANSACTION_INTRINSIC_GAS
        .saturating_add(transact)
        .saturating_add(relay)
        .saturating_add(RELAY_NATIVE_UNWRAP_GAS.saturating_mul(shape.unwrap_count as u64))
        .saturating_add(executor)
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

/// The live fee check adds `gas_limit_buffer` to the RPC estimate, so the quote adds it too.
pub(crate) fn approximate_public_broadcaster_cost(
    broadcaster: PublicBroadcasterCandidate,
    gas_limit_buffer: u64,
    action_token: Address,
    fee_token: Address,
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
    protocol_fee_bps: U256,
    min_gas_price: u128,
    initial_fee_amount: U256,
    custom_fee_amount: Option<U256>,
    mut select_shape: impl FnMut(PublicBroadcasterAmountSplit) -> Result<ApproximateTransactionShape>,
) -> Result<PublicBroadcasterCostEstimate> {
    let service_gas_price = public_broadcaster_service_gas_price(min_gas_price);
    let model = RailgunGasModel::for_chain(broadcaster.chain_id);
    let mut fee_amount = custom_fee_amount.map_or(Ok(initial_fee_amount), |amount| {
        validate_custom_public_broadcaster_fee(amount, U256::ZERO, None)
    })?;
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
        let gas_limit = approximate_public_broadcaster_gas(model, GasEstimateMode::Expected, shape)
            .saturating_add(gas_limit_buffer);
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
        fee_amount = custom_fee_amount.map_or_else(
            || Ok(buffered_public_broadcaster_fee(computed_fee)),
            |amount| validate_custom_public_broadcaster_fee(amount, computed_fee, None),
        )?;
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

/// Like [`approximate_public_broadcaster_cost`], the seed fee includes `gas_limit_buffer`.
pub(crate) fn initial_separate_token_public_broadcaster_fee(
    broadcaster: &PublicBroadcasterCandidate,
    gas_limit_buffer: u64,
    min_gas_price: u128,
    seed_shape: ApproximateTransactionShape,
) -> U256 {
    let service_gas_price = public_broadcaster_service_gas_price(min_gas_price);
    let gas_limit = approximate_public_broadcaster_gas(
        RailgunGasModel::for_chain(broadcaster.chain_id),
        GasEstimateMode::Expected,
        seed_shape,
    )
    .saturating_add(gas_limit_buffer);
    buffered_public_broadcaster_fee(broadcaster_fee_amount(
        broadcaster.fee,
        gas_limit,
        service_gas_price,
    ))
}

pub(super) fn initial_public_broadcaster_fee_amount(
    broadcaster: &PublicBroadcasterCandidate,
    gas_limit_buffer: u64,
    min_gas_price: u128,
    same_token_fee: bool,
    seed_shape: impl FnOnce() -> Result<ApproximateTransactionShape>,
) -> Result<U256> {
    if same_token_fee {
        Ok(U256::ZERO)
    } else {
        Ok(initial_separate_token_public_broadcaster_fee(
            broadcaster,
            gas_limit_buffer,
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
        executor: false,
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
        executor: false,
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
            executor: false,
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
        executor: false,
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
