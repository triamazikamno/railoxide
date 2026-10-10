use super::*;
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BlockedShieldRescueInfo {
    pub eligible: bool,
    pub disabled_reason: Option<String>,
    pub origin_address: Option<String>,
    pub public_account_uuid: Option<String>,
    pub public_account_label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityUtxoClassification {
    Shield,
    BlockedShield,
    PrivateOutput,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum UtxoPpoiState {
    Valid,
    Missing,
    ProofSubmitted,
    Unknown,
    ShieldBlocked,
    Mixed,
}

impl UtxoPpoiState {
    #[must_use]
    pub const fn retry_eligible(self) -> bool {
        matches!(
            self,
            Self::Missing | Self::ProofSubmitted | Self::Unknown | Self::Mixed
        )
    }
}

impl ActivityUtxoClassification {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Shield => "Shield",
            Self::BlockedShield => "Blocked Shield",
            Self::PrivateOutput => "Private Output",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UtxoOutput {
    pub tree: u32,
    pub position: u64,
    pub token: String,
    pub value: String,
    pub commitment_kind: String,
    pub activity_classification: String,
    pub blocked_shield_rescue: Option<BlockedShieldRescueInfo>,
    pub commitment: String,
    pub npk: String,
    pub blinded_commitment: String,
    pub poi_statuses: BTreeMap<String, String>,
    pub ppoi_state: UtxoPpoiState,
    pub ppoi_last_submission_at: Option<u64>,
    pub poi_spendable: bool,
    pub source_tx_hash: String,
    pub source_block_number: u64,
    pub source_block_timestamp: u64,
    pub is_spent: bool,
    pub pending_new: bool,
    pub pending_spent: bool,
    pub local_pending_spent: bool,
    pub spent_tx_hash: Option<String>,
    pub spent_block_number: Option<u64>,
}

impl UtxoOutput {
    #[must_use]
    pub fn is_ppoi_retry_eligible(&self) -> bool {
        !self.is_spent
            && !self.pending_new
            && !self.pending_spent
            && !self.local_pending_spent
            && self.commitment_kind == "Transact"
            && self.ppoi_state.retry_eligible()
    }

    fn planner_utxo_for_token(&self, token: Address) -> Option<Utxo> {
        if self.is_spent
            || self.pending_new
            || self.pending_spent
            || self.local_pending_spent
            || !self.poi_spendable
        {
            return None;
        }
        let row_token = self.token.parse::<Address>().ok()?;
        if row_token != token {
            return None;
        }
        let value = U256::from_str_radix(&self.value, 10).ok()?;
        if value.is_zero() {
            return None;
        }
        // Max-spendable selection only reads the note token/value and tree/position.
        // Avoid Utxo::new here because it computes two unused Poseidon commitments.
        Some(Utxo {
            note: Note::new_unshield(Address::ZERO, token, value),
            tree: self.tree,
            position: self.position,
            source: UtxoSource {
                tx_hash: FixedBytes::ZERO,
                block_number: self.source_block_number,
                block_timestamp: self.source_block_timestamp,
            },
            poi: UtxoPoiMetadata {
                commitment_kind: UtxoCommitmentKind::Transact,
                commitment: FixedBytes::ZERO,
                npk: FixedBytes::ZERO,
                blinded_commitment: FixedBytes::ZERO,
                statuses: BTreeMap::new(),
                refreshed_at: None,
            },
        })
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TokenTotal {
    pub token: String,
    pub total: String,
    pub poi_verified_total: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ListUtxosOutput {
    pub chain_id: u64,
    pub cache_key: String,
    pub utxo_count: usize,
    pub unspent_count: usize,
    pub spent_count: usize,
    pub local_pending_spent_count: usize,
    pub utxos: Vec<UtxoOutput>,
    pub totals: Vec<TokenTotal>,
}

#[must_use]
pub fn max_unshield_amount_from_outputs(utxos: &[UtxoOutput], token: Address) -> U256 {
    let planner_utxos = planner_utxos_from_outputs(utxos, token);
    max_unshield_spendable(&planner_utxos, token)
}

#[must_use]
pub fn max_send_amount_from_outputs(utxos: &[UtxoOutput], token: Address) -> U256 {
    let planner_utxos = planner_utxos_from_outputs(utxos, token);
    max_send_spendable(&planner_utxos, token)
}

#[must_use]
pub fn max_broadcaster_fee_token_amount_from_outputs(utxos: &[UtxoOutput], token: Address) -> U256 {
    let planner_utxos = planner_utxos_from_outputs(utxos, token);
    max_broadcaster_fee_token_spendable(&planner_utxos, token)
}

fn planner_utxos_from_outputs(utxos: &[UtxoOutput], token: Address) -> Vec<Utxo> {
    utxos
        .iter()
        .filter_map(|row| row.planner_utxo_for_token(token))
        .collect()
}

#[must_use]
pub(crate) fn utxo_outputs_from_utxos(
    mut utxos: Vec<WalletUtxo>,
) -> (Vec<UtxoOutput>, Vec<TokenTotal>) {
    utxos.sort_by(|a, b| match a.utxo.tree.cmp(&b.utxo.tree) {
        std::cmp::Ordering::Equal => a.utxo.position.cmp(&b.utxo.position),
        other => other,
    });

    let active_poi_list_keys = default_active_poi_list_keys();
    let mut totals_map: BTreeMap<Address, U256> = BTreeMap::new();
    let mut poi_verified_totals_map: BTreeMap<Address, U256> = BTreeMap::new();
    let utxo_outputs = utxos
        .into_iter()
        .map(|wallet_utxo| {
            let utxo = wallet_utxo.utxo;
            let token_addr = utxo.token_address();
            let poi_spendable =
                wallet_utxo.spent.is_none() && utxo.poi.is_valid_for_lists(&active_poi_list_keys);
            if wallet_utxo.spent.is_none() {
                *totals_map.entry(token_addr).or_default() += utxo.note.value;
            }
            if poi_spendable {
                *poi_verified_totals_map.entry(token_addr).or_default() += utxo.note.value;
            }
            let source = &utxo.source;
            let spent = wallet_utxo.spent.as_ref();
            let activity_classification =
                activity_utxo_classification(&utxo.poi, &active_poi_list_keys);
            let poi_statuses = poi_statuses_for_output(&utxo, &active_poi_list_keys);
            let ppoi_state = utxo_ppoi_state(&utxo.poi, &active_poi_list_keys);

            UtxoOutput {
                tree: utxo.tree,
                position: utxo.position,
                token: token_addr.to_checksum(None),
                value: utxo.note.value.to_string(),
                commitment_kind: commitment_kind_label(utxo.poi.commitment_kind).to_string(),
                activity_classification: activity_classification.label().to_string(),
                blocked_shield_rescue: blocked_shield_rescue_info(
                    activity_classification,
                    wallet_utxo.spent.is_some(),
                    false,
                    false,
                    false,
                ),
                commitment: hex::encode_prefixed(utxo.poi.commitment),
                npk: hex::encode_prefixed(utxo.poi.npk),
                blinded_commitment: hex::encode_prefixed(utxo.poi.blinded_commitment),
                poi_statuses,
                ppoi_state,
                ppoi_last_submission_at: None,
                poi_spendable,
                source_tx_hash: hex::encode_prefixed(source.tx_hash),
                source_block_number: source.block_number,
                source_block_timestamp: source.block_timestamp,
                is_spent: wallet_utxo.spent.is_some(),
                pending_new: false,
                pending_spent: false,
                local_pending_spent: false,
                spent_tx_hash: spent.map(|source| hex::encode_prefixed(source.tx_hash)),
                spent_block_number: spent.map(|source| source.block_number),
            }
        })
        .collect();

    let totals = totals_map
        .into_iter()
        .map(|(addr, total)| TokenTotal {
            token: addr.to_checksum(None),
            total: total.to_string(),
            poi_verified_total: poi_verified_totals_map
                .remove(&addr)
                .unwrap_or_default()
                .to_string(),
        })
        .collect();

    (utxo_outputs, totals)
}

pub(crate) fn apply_pending_overlay_to_outputs(
    confirmed_utxos: &[WalletUtxo],
    overlay: WalletPendingOverlay,
    outputs: &mut Vec<UtxoOutput>,
) {
    let confirmed_spent: HashSet<_> = confirmed_utxos
        .iter()
        .filter(|utxo| utxo.is_spent())
        .map(|utxo| (utxo.utxo.tree, utxo.utxo.position))
        .collect();
    let local_pending_spent = overlay.local_pending_spent;
    let mut pending_spent = BTreeMap::new();
    for spent in overlay.pending_spent {
        pending_spent.insert(spent.key(), spent);
    }

    for output in outputs.iter_mut() {
        if output.is_spent {
            continue;
        }
        if let Some(spent) = pending_spent.get(&(output.tree, output.position)) {
            mark_output_pending_spent(output, spent);
        } else if let Some(spent) = local_pending_spent.iter().find(|spent| {
            spent.key() == (output.tree, output.position)
                && confirmed_utxos.iter().any(|utxo| {
                    !utxo.is_spent()
                        && utxo.utxo.tree == output.tree
                        && utxo.utxo.position == output.position
                        && spent.matches_local_utxo(utxo)
                })
        }) {
            mark_output_local_pending_spent(output, spent);
        }
    }

    for pending in overlay.new_utxos {
        if confirmed_spent.contains(&(pending.utxo.tree, pending.utxo.position))
            || outputs.iter().any(|output| {
                output.tree == pending.utxo.tree && output.position == pending.utxo.position
            })
        {
            continue;
        }
        outputs.push(pending_utxo_output(pending));
    }
}

fn mark_output_pending_spent(output: &mut UtxoOutput, spent: &WalletPendingSpent) {
    output.pending_spent = true;
    output.poi_spendable = false;
    if output.blocked_shield_rescue.is_some() {
        output.blocked_shield_rescue = blocked_shield_rescue_info(
            ActivityUtxoClassification::BlockedShield,
            output.is_spent,
            output.pending_new,
            true,
            output.local_pending_spent,
        );
    }
    if output.spent_tx_hash.is_none() {
        output.spent_tx_hash = spent.tx_hash.map(hex::encode_prefixed);
    }
    if output.spent_block_number.is_none() {
        output.spent_block_number = spent.block_number;
    }
}

fn mark_output_local_pending_spent(output: &mut UtxoOutput, spent: &WalletPendingSpent) {
    output.local_pending_spent = true;
    output.poi_spendable = false;
    if output.blocked_shield_rescue.is_some() {
        output.blocked_shield_rescue = blocked_shield_rescue_info(
            ActivityUtxoClassification::BlockedShield,
            output.is_spent,
            output.pending_new,
            output.pending_spent,
            true,
        );
    }
    if output.spent_tx_hash.is_none() {
        output.spent_tx_hash = spent.tx_hash.map(hex::encode_prefixed);
    }
}

fn pending_utxo_output(wallet_utxo: WalletUtxo) -> UtxoOutput {
    let utxo = wallet_utxo.utxo;
    let token_addr = utxo.token_address();
    let spent = wallet_utxo.spent.as_ref();
    let source = &utxo.source;
    let activity_classification =
        activity_utxo_classification(&utxo.poi, &default_active_poi_list_keys());
    UtxoOutput {
        tree: utxo.tree,
        position: utxo.position,
        token: token_addr.to_checksum(None),
        value: utxo.note.value.to_string(),
        commitment_kind: commitment_kind_label(utxo.poi.commitment_kind).to_string(),
        activity_classification: activity_classification.label().to_string(),
        blocked_shield_rescue: blocked_shield_rescue_info(
            activity_classification,
            false,
            true,
            spent.is_some(),
            false,
        ),
        commitment: hex::encode_prefixed(utxo.poi.commitment),
        npk: hex::encode_prefixed(utxo.poi.npk),
        blinded_commitment: hex::encode_prefixed(utxo.poi.blinded_commitment),
        poi_statuses: BTreeMap::new(),
        ppoi_state: UtxoPpoiState::Unknown,
        ppoi_last_submission_at: None,
        poi_spendable: false,
        source_tx_hash: hex::encode_prefixed(source.tx_hash),
        source_block_number: source.block_number,
        source_block_timestamp: source.block_timestamp,
        is_spent: false,
        pending_new: true,
        pending_spent: spent.is_some(),
        local_pending_spent: false,
        spent_tx_hash: spent.map(|source| hex::encode_prefixed(source.tx_hash)),
        spent_block_number: spent.map(|source| source.block_number),
    }
}

const fn commitment_kind_label(kind: UtxoCommitmentKind) -> &'static str {
    match kind {
        UtxoCommitmentKind::Shield => "Shield",
        UtxoCommitmentKind::Transact => "Transact",
    }
}

#[must_use]
pub(crate) fn activity_utxo_classification(
    poi: &UtxoPoiMetadata,
    active_poi_list_keys: &[FixedBytes<32>],
) -> ActivityUtxoClassification {
    match poi.commitment_kind {
        UtxoCommitmentKind::Shield => {
            if active_poi_list_keys
                .iter()
                .any(|list_key| poi.statuses.get(list_key) == Some(&PoiStatus::ShieldBlocked))
            {
                ActivityUtxoClassification::BlockedShield
            } else {
                ActivityUtxoClassification::Shield
            }
        }
        UtxoCommitmentKind::Transact => ActivityUtxoClassification::PrivateOutput,
    }
}

fn blocked_shield_rescue_info(
    classification: ActivityUtxoClassification,
    is_spent: bool,
    pending_new: bool,
    pending_spent: bool,
    local_pending_spent: bool,
) -> Option<BlockedShieldRescueInfo> {
    if classification != ActivityUtxoClassification::BlockedShield {
        return None;
    }
    let disabled_reason = if is_spent {
        Some("Spent blocked Shield UTXOs cannot be refunded.".to_string())
    } else if pending_spent || local_pending_spent {
        Some("This blocked Shield UTXO is already pending spend.".to_string())
    } else if pending_new {
        Some("Pending received blocked Shield UTXOs cannot be refunded yet.".to_string())
    } else {
        Some("Source transaction origin has not been resolved yet.".to_string())
    };

    Some(BlockedShieldRescueInfo {
        eligible: false,
        disabled_reason,
        origin_address: None,
        public_account_uuid: None,
        public_account_label: None,
    })
}

const fn poi_status_label(status: PoiStatus) -> &'static str {
    match status {
        PoiStatus::Valid => "Valid",
        PoiStatus::ShieldBlocked => "ShieldBlocked",
        PoiStatus::ProofSubmitted => "ProofSubmitted",
        PoiStatus::Missing => "Missing",
        PoiStatus::Unknown => "Unknown",
    }
}

pub(crate) fn utxo_ppoi_state(
    poi: &UtxoPoiMetadata,
    active_poi_list_keys: &[FixedBytes<32>],
) -> UtxoPpoiState {
    let mut state = None;
    let mut mixed = false;
    for list_key in active_poi_list_keys {
        let current = match poi.statuses.get(list_key) {
            Some(PoiStatus::Valid) => UtxoPpoiState::Valid,
            Some(PoiStatus::ShieldBlocked) => return UtxoPpoiState::ShieldBlocked,
            Some(PoiStatus::ProofSubmitted) => UtxoPpoiState::ProofSubmitted,
            Some(PoiStatus::Missing) => UtxoPpoiState::Missing,
            Some(PoiStatus::Unknown) | None => UtxoPpoiState::Unknown,
        };
        match state {
            None => state = Some(current),
            Some(previous) if previous == current => {}
            Some(_) => mixed = true,
        }
    }
    if mixed {
        UtxoPpoiState::Mixed
    } else {
        state.unwrap_or(UtxoPpoiState::Unknown)
    }
}

fn poi_statuses_for_output(
    utxo: &Utxo,
    active_poi_list_keys: &[FixedBytes<32>],
) -> BTreeMap<String, String> {
    let mut statuses = utxo.poi.statuses.clone();
    for list_key in active_poi_list_keys {
        statuses.entry(*list_key).or_insert(PoiStatus::Unknown);
    }
    statuses
        .into_iter()
        .map(|(list_key, status)| (hex::encode(list_key), poi_status_label(status).to_string()))
        .collect()
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

pub(crate) fn chain_pending_spent_keys(
    pending_overlay: &WalletPendingOverlay,
) -> HashSet<(u32, u64)> {
    pending_overlay
        .pending_spent
        .iter()
        .map(WalletPendingSpent::key)
        .collect()
}
