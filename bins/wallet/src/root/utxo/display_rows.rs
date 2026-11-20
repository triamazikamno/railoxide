#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::root) fn display_rows_from_output(
    output: &ListUtxosOutput,
    tx_query: &str,
    show_spent_utxos: bool,
) -> Vec<UtxoDisplayRow> {
    let tx_query = tx_query.trim().to_ascii_lowercase();
    let mut rows: Vec<_> = output
        .utxos
        .iter()
        .filter(|row| matches_utxo_filters(row, &tx_query, show_spent_utxos))
        .map(|row| display_row_from_utxo(output.chain_id, row))
        .collect();
    rows.reverse();
    rows
}

pub(in crate::root) fn blocked_shield_rescue_display_rows(
    output: &ListUtxosOutput,
    rescue_rows: &std::collections::BTreeMap<
        BlockedShieldRescueUtxoId,
        BlockedShieldRescueRowState,
    >,
    in_flight_refunds: &BTreeSet<BlockedShieldRescueUtxoId>,
) -> Vec<UtxoDisplayRow> {
    let mut rows = display_rows_from_output(output, "", false);
    apply_blocked_shield_rescue_rows(&mut rows, rescue_rows, in_flight_refunds);
    rows.into_iter()
        .filter(should_show_blocked_shield_refund_action)
        .collect()
}

pub(in crate::root) fn apply_blocked_shield_rescue_rows(
    rows: &mut [UtxoDisplayRow],
    rescue_rows: &std::collections::BTreeMap<
        BlockedShieldRescueUtxoId,
        BlockedShieldRescueRowState,
    >,
    in_flight_refunds: &BTreeSet<BlockedShieldRescueUtxoId>,
) {
    for row in rows {
        let Some(utxo_id) = row.utxo_id else {
            continue;
        };
        if !accepts_blocked_shield_rescue_overlay(row) {
            continue;
        }
        if let Some(rescue) = rescue_rows.get(&utxo_id) {
            row.blocked_shield_rescue = Some(rescue.info().clone());
        }
        if in_flight_refunds.contains(&utxo_id) {
            row.blocked_shield_rescue = Some(blocked_shield_rescue_in_flight_info(
                row.blocked_shield_rescue.as_ref(),
            ));
        }
    }
}

const fn accepts_blocked_shield_rescue_overlay(row: &UtxoDisplayRow) -> bool {
    row.blocked_shield_rescue.is_some()
        && !row.is_spent
        && !row.pending_new
        && !row.pending_spent
        && !row.local_pending_spent
}

pub(super) fn blocked_shield_rescue_utxo_id_from_output(
    row: &UtxoOutput,
) -> Option<BlockedShieldRescueUtxoId> {
    row.blocked_shield_rescue.as_ref()?;
    Some(BlockedShieldRescueUtxoId {
        tree: row.tree,
        position: row.position,
        commitment: parse_fixed_bytes_32(&row.commitment)?,
        blinded_commitment: parse_fixed_bytes_32(&row.blinded_commitment)?,
    })
}

pub(super) fn active_blocked_shield_rescue_utxo_id_from_output(
    row: &UtxoOutput,
) -> Option<BlockedShieldRescueUtxoId> {
    if row.is_spent || row.pending_new || row.pending_spent || row.local_pending_spent {
        return None;
    }
    blocked_shield_rescue_utxo_id_from_output(row)
}

pub(super) fn can_start_blocked_shield_origin_resolution(
    row: &UtxoDisplayRow,
    rescue: &BlockedShieldRescueInfo,
) -> bool {
    accepts_blocked_shield_rescue_overlay(row)
        && !rescue.eligible
        && rescue.disabled_reason.as_deref() != Some(BLOCKED_SHIELD_RESCUE_RESOLVING_REASON)
        && rescue.disabled_reason.as_deref() != Some(BLOCKED_SHIELD_REFUND_IN_FLIGHT_REASON)
        && rescue.disabled_reason.as_deref() != Some(BLOCKED_SHIELD_REFUND_SUBMITTED_REASON)
}

fn parse_fixed_bytes_32(value: &str) -> Option<FixedBytes<32>> {
    let bare = value.strip_prefix("0x").unwrap_or(value);
    hex::decode_to_array(bare).ok().map(FixedBytes::from)
}

pub(super) fn blocked_shield_rescue_info_from_eligibility(
    eligibility: wallet_ops::BlockedShieldRescueEligibility,
) -> BlockedShieldRescueInfo {
    BlockedShieldRescueInfo {
        eligible: eligibility.eligible,
        disabled_reason: eligibility.disabled_reason,
        origin_address: eligibility
            .origin_address
            .map(|address| address.to_checksum(None)),
        public_account_uuid: eligibility.public_account_uuid,
        public_account_label: eligibility.public_account_label,
    }
}

fn matches_utxo_filters(row: &UtxoOutput, tx_query: &str, show_spent_utxos: bool) -> bool {
    if tx_query.is_empty() {
        return show_spent_utxos || !row.is_spent || row.pending_spent || row.local_pending_spent;
    }

    row.source_tx_hash.to_ascii_lowercase().contains(tx_query)
        || row
            .spent_tx_hash
            .as_deref()
            .is_some_and(|hash| hash.to_ascii_lowercase().contains(tx_query))
}

pub(super) fn display_row_from_utxo(chain_id: u64, row: &UtxoOutput) -> UtxoDisplayRow {
    let raw_value = U256::from_str_radix(&row.value, 10).ok();
    let Some(address) = parse_address(&row.token) else {
        return UtxoDisplayRow {
            utxo_id: blocked_shield_rescue_utxo_id_from_output(row),
            tree_position: format_tree_position(row.tree, row.position),
            token: row.token.clone(),
            token_icon_path: None,
            amount: row.value.clone(),
            raw_value,
            activity_classification: row.activity_classification.clone(),
            poi_status: format_poi_status(row),
            ppoi_state: row.ppoi_state,
            ppoi_last_submission_at: row.ppoi_last_submission_at,
            poi_spendable: row.poi_spendable,
            source_tx_hash: row.source_tx_hash.clone(),
            source_block_number: row.source_block_number,
            source_block_timestamp: row.source_block_timestamp,
            spent_tx_hash: row.spent_tx_hash.clone(),
            spent_block_number: row.spent_block_number,
            token_address: row.token.clone(),
            is_spent: row.is_spent,
            pending_new: row.pending_new,
            pending_spent: row.pending_spent,
            local_pending_spent: row.local_pending_spent,
            blocked_shield_rescue: row.blocked_shield_rescue.clone(),
        };
    };

    let (token, amount, token_icon_path) = if let Some(token) = lookup_token(chain_id, &address) {
        let amount = raw_value.map_or_else(
            || row.value.clone(),
            |value| format_token_amount(value, token.decimals),
        );
        (
            token.symbol.to_owned(),
            amount,
            token_icon_asset_path(chain_id, &address).map(WalletIconSource::embedded),
        )
    } else {
        (short_address(&address), row.value.clone(), None)
    };

    UtxoDisplayRow {
        utxo_id: blocked_shield_rescue_utxo_id_from_output(row),
        tree_position: format_tree_position(row.tree, row.position),
        token,
        token_icon_path,
        amount,
        raw_value,
        activity_classification: row.activity_classification.clone(),
        poi_status: format_poi_status(row),
        ppoi_state: row.ppoi_state,
        ppoi_last_submission_at: row.ppoi_last_submission_at,
        poi_spendable: row.poi_spendable,
        source_tx_hash: row.source_tx_hash.clone(),
        source_block_number: row.source_block_number,
        source_block_timestamp: row.source_block_timestamp,
        spent_tx_hash: row.spent_tx_hash.clone(),
        spent_block_number: row.spent_block_number,
        token_address: address.to_checksum(None),
        is_spent: row.is_spent,
        pending_new: row.pending_new,
        pending_spent: row.pending_spent,
        local_pending_spent: row.local_pending_spent,
        blocked_shield_rescue: row.blocked_shield_rescue.clone(),
    }
}

fn format_poi_status(row: &UtxoOutput) -> String {
    if row.pending_spent {
        return "Pending spend".to_string();
    }
    if row.local_pending_spent {
        return "Locally locked".to_string();
    }
    if row.pending_new {
        return "Pending receive".to_string();
    }
    match row.ppoi_state {
        UtxoPpoiState::Valid => "Valid",
        UtxoPpoiState::Missing => "Missing",
        UtxoPpoiState::ProofSubmitted => "ProofSubmitted",
        UtxoPpoiState::Unknown => "Unknown",
        UtxoPpoiState::ShieldBlocked => "ShieldBlocked",
        UtxoPpoiState::Mixed => "Mixed",
    }
    .to_string()
}

fn format_tree_position(tree: u32, position: u64) -> String {
    format!("{tree}/{position}")
}

pub(super) fn generated_age_label(timestamp: u64) -> String {
    let age_secs = now_epoch_secs().saturating_sub(timestamp);
    ui::format::format_relative_age(Duration::from_secs(age_secs))
}

pub(in crate::root) fn format_ppoi_submission_age(timestamp: u64, now_epoch_secs: u64) -> String {
    let age_secs = now_epoch_secs.saturating_sub(timestamp);
    if age_secs < SECONDS_PER_HOUR {
        let age = if age_secs < SECONDS_PER_MINUTE {
            format!("{age_secs}s")
        } else {
            let minutes = age_secs / SECONDS_PER_MINUTE;
            let seconds = age_secs % SECONDS_PER_MINUTE;
            if seconds == 0 {
                format!("{minutes}m")
            } else {
                format!("{minutes}m {seconds}s")
            }
        };
        return format!("Submitted {age} ago");
    }

    format!(
        "Submitted {}",
        ui::format::format_relative_age(Duration::from_secs(age_secs))
    )
}

pub(super) fn local_datetime_label(timestamp: u64) -> String {
    let Ok(seconds) = i64::try_from(timestamp) else {
        return format!("Unix timestamp {timestamp}");
    };
    let Some(utc) = DateTime::<Utc>::from_timestamp(seconds, 0) else {
        return format!("Unix timestamp {timestamp}");
    };
    utc.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S %Z")
        .to_string()
}

pub(in crate::root) fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(in crate::root) fn short_hash(hash: &str) -> String {
    if hash.len() <= 14 {
        return hash.to_string();
    }
    format!("{}...{}", &hash[..8], &hash[hash.len() - 6..])
}
