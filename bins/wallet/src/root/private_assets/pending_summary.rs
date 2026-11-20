#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn apply_pending_asset_amounts(
    snapshot: &ListUtxosOutput,
    registry: Option<&EffectiveTokenRegistry>,
    anchor_cache: Option<&TokenAnchorRateCache>,
    rows: &mut Vec<FormattedTokenTotal>,
) {
    let mut pending: BTreeMap<Address, (U256, U256)> = BTreeMap::new();
    for row in &snapshot.utxos {
        if !row.pending_new && !row.pending_spent && !row.local_pending_spent {
            continue;
        }
        let Some(token) = parse_address(&row.token) else {
            continue;
        };
        let Ok(value) = U256::from_str_radix(&row.value, 10) else {
            continue;
        };
        let entry = pending.entry(token).or_default();
        if row.pending_new {
            entry.0 += value;
        }
        if row.pending_spent || row.local_pending_spent {
            entry.1 += value;
        }
    }

    for (token, (incoming, outgoing)) in pending {
        let index = rows.iter().position(|row| row.token == Some(token));
        let row = if let Some(index) = index {
            &mut rows[index]
        } else {
            rows.push(format_total_parts(
                snapshot.chain_id,
                &TokenTotal {
                    token: token.to_checksum(None),
                    total: "0".to_string(),
                    poi_verified_total: "0".to_string(),
                },
                registry,
                anchor_cache,
            ));
            rows.last_mut().expect("pending row inserted")
        };
        row.pending_incoming_total = Some(incoming);
        row.pending_outgoing_total = Some(outgoing);
        row.pending_incoming_amount = format_pending_amount(incoming, row.decimals);
        row.pending_outgoing_amount = format_pending_amount(outgoing, row.decimals);
    }
}

pub(super) fn private_asset_usd_micro_amount(
    chain_id: u64,
    token: Address,
    total: U256,
    anchor_cache: Option<&TokenAnchorRateCache>,
) -> Option<U256> {
    anchor_cache.and_then(|cache| cache.cached_token_usd_micro_value(chain_id, token, total))
}

pub(super) fn pending_poi_total(
    total: Option<U256>,
    poi_verified_total: Option<U256>,
) -> Option<U256> {
    total
        .zip(poi_verified_total)
        .map(|(total, poi_verified_total)| total.saturating_sub(poi_verified_total))
}

pub(super) fn format_pending_poi_amount(
    pending_poi_total: Option<U256>,
    decimals: Option<u8>,
) -> String {
    pending_poi_total.as_ref().map_or_else(
        || "0".to_string(),
        |value| {
            if let Some(decimals) = decimals {
                format_token_amount(*value, decimals)
            } else {
                value.to_string()
            }
        },
    )
}

fn format_pending_amount(value: U256, decimals: Option<u8>) -> String {
    decimals.map_or_else(
        || value.to_string(),
        |decimals| format_token_amount(value, decimals),
    )
}

pub(in crate::root) fn should_show_pending_poi_amount(pending_poi_total: Option<U256>) -> bool {
    pending_poi_total.is_some_and(|amount| !amount.is_zero())
}

pub(in crate::root) fn should_show_pending_amount(pending_total: Option<U256>) -> bool {
    pending_total.is_some_and(|amount| !amount.is_zero())
}

#[cfg(test)]
pub(in crate::root) fn private_pending_summary(
    assets: &[FormattedTokenTotal],
    snapshot: &ListUtxosOutput,
    blocked_shield_rows: Vec<UtxoDisplayRow>,
    poi_refreshing: bool,
    poi_refresh_session: Option<Arc<wallet_ops::WalletSession>>,
) -> Option<PrivatePendingSummary> {
    private_pending_summary_with_workflow(
        assets,
        snapshot,
        blocked_shield_rows,
        WalletPpoiWorkflowStatus::default(),
        poi_refreshing,
        poi_refresh_session,
    )
}

pub(in crate::root) fn private_pending_summary_with_workflow(
    assets: &[FormattedTokenTotal],
    snapshot: &ListUtxosOutput,
    blocked_shield_rows: Vec<UtxoDisplayRow>,
    ppoi_workflow_status: WalletPpoiWorkflowStatus,
    poi_refreshing: bool,
    poi_refresh_session: Option<Arc<wallet_ops::WalletSession>>,
) -> Option<PrivatePendingSummary> {
    let shield_waits = pending_shield_waits_by_token(snapshot, now_epoch_secs());
    let pending_poi_assets = assets
        .iter()
        .filter(|asset| should_show_pending_poi_amount(asset.pending_poi_total))
        .map(|asset| PrivatePendingAssetLine {
            label: asset.label.clone(),
            amount: asset.pending_poi_amount.clone(),
            shield_wait: asset
                .token
                .and_then(|token| shield_waits.get(&token).copied())
                .filter(|wait| pending_shield_wait_matches_total(*wait, asset.pending_poi_total)),
        })
        .collect::<Vec<_>>();
    let pending_incoming_assets = assets
        .iter()
        .filter(|asset| should_show_pending_amount(asset.pending_incoming_total))
        .map(|asset| PrivatePendingAssetLine {
            label: asset.label.clone(),
            amount: asset.pending_incoming_amount.clone(),
            shield_wait: None,
        })
        .collect::<Vec<_>>();
    let pending_outgoing_assets = assets
        .iter()
        .filter(|asset| should_show_pending_amount(asset.pending_outgoing_total))
        .map(|asset| PrivatePendingAssetLine {
            label: asset.label.clone(),
            amount: asset.pending_outgoing_amount.clone(),
            shield_wait: None,
        })
        .collect::<Vec<_>>();
    let affected_asset_count = assets
        .iter()
        .filter(|asset| {
            should_show_pending_poi_amount(asset.pending_poi_total)
                || should_show_pending_amount(asset.pending_incoming_total)
                || should_show_pending_amount(asset.pending_outgoing_total)
                || asset.token.is_some_and(|token| {
                    snapshot.utxos.iter().any(|row| {
                        row.is_ppoi_retry_eligible() && parse_address(&row.token) == Some(token)
                    })
                })
        })
        .count();
    let recoverable_poi_outputs = recoverable_poi_candidate_count(snapshot);
    let mut recoverable_ppoi_states = Vec::new();
    for state in snapshot
        .utxos
        .iter()
        .filter(|row| row.is_ppoi_retry_eligible())
        .map(|row| row.ppoi_state)
    {
        if !recoverable_ppoi_states.contains(&state) {
            recoverable_ppoi_states.push(state);
        }
    }

    if pending_poi_assets.is_empty()
        && pending_incoming_assets.is_empty()
        && pending_outgoing_assets.is_empty()
        && blocked_shield_rows.is_empty()
        && recoverable_poi_outputs == 0
        && !ppoi_workflow_status.has_outstanding()
    {
        return None;
    }

    Some(PrivatePendingSummary {
        blocked_shield_rows,
        pending_poi_assets,
        pending_incoming_assets,
        pending_outgoing_assets,
        affected_asset_count,
        recoverable_poi_outputs,
        recoverable_ppoi_states,
        ppoi_workflow_status,
        poi_refreshing,
        poi_refresh_session,
    })
}

pub(in crate::root) fn pending_shield_waits_by_token(
    snapshot: &ListUtxosOutput,
    now_epoch_secs: u64,
) -> BTreeMap<Address, PrivatePendingShieldWait> {
    let mut waits = BTreeMap::new();
    for row in display_rows_from_output(snapshot, "", false) {
        let Some(display) = shield_poi_wait_display(&row, now_epoch_secs) else {
            continue;
        };
        let Some(token) = parse_address(&row.token_address) else {
            continue;
        };
        let wait = waits.entry(token).or_insert(PrivatePendingShieldWait {
            output_count: 0,
            latest_source_block_timestamp: 0,
            total_value: Some(U256::ZERO),
            has_delayed: false,
        });
        wait.output_count += 1;
        wait.latest_source_block_timestamp = wait
            .latest_source_block_timestamp
            .max(row.source_block_timestamp);
        wait.total_value = wait
            .total_value
            .and_then(|total| row.raw_value.and_then(|value| total.checked_add(value)));
        wait.has_delayed |= display.delayed;
    }
    waits
}

pub(in crate::root) fn pending_shield_wait_matches_total(
    wait: PrivatePendingShieldWait,
    pending_poi_total: Option<U256>,
) -> bool {
    wait.total_value
        .is_some_and(|total_value| pending_poi_total == Some(total_value))
}

pub(in crate::root) const fn private_pending_summary_detail(
    summary: &PrivatePendingSummary,
) -> Option<&'static str> {
    if summary.blocked_shield_rows.is_empty() {
        None
    } else if summary.blocked_shield_rows.len() == 1 {
        Some("A blocked Shield can be reviewed and refunded.")
    } else {
        Some("Blocked Shields can be reviewed and refunded.")
    }
}

pub(in crate::root) fn private_pending_summary_title(summary: &PrivatePendingSummary) -> String {
    if !summary.blocked_shield_rows.is_empty() {
        "Private assets need attention".to_string()
    } else if !summary.pending_poi_assets.is_empty()
        || !summary.pending_incoming_assets.is_empty()
        || summary.recoverable_poi_outputs > 0
    {
        format!(
            "{} not yet spendable",
            pending_asset_count_label(summary.affected_asset_count)
        )
    } else if summary.ppoi_workflow_status.needs_attention > 0
        || summary.ppoi_workflow_status.recovery_needs_attention > 0
    {
        "Outgoing proof recovery needs attention".to_string()
    } else if summary.ppoi_workflow_status.awaiting_public_txid_data > 0 {
        "Waiting for public transaction proof data".to_string()
    } else if summary.ppoi_workflow_status.awaiting_poi_data > 0 {
        "Waiting for PPOI data".to_string()
    } else if summary.ppoi_workflow_status.awaiting_validation > 0
        && summary.ppoi_workflow_status.awaiting_recovery == 0
        && summary.ppoi_workflow_status.awaiting_submission == 0
    {
        "Awaiting outgoing proof verification".to_string()
    } else if summary.ppoi_workflow_status.has_outstanding() {
        "Outgoing proof recovery pending".to_string()
    } else {
        format!(
            "{} awaiting confirmation",
            pending_asset_count_label(summary.affected_asset_count)
        )
    }
}
