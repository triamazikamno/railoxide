#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn is_shield_blocked_poi_status(status: &str) -> bool {
    status == "ShieldBlocked"
}

pub(super) fn utxo_cell_text_color(row: &UtxoDisplayRow, color: gpui::Rgba) -> gpui::Rgba {
    if row.is_spent {
        rgb(theme::SPENT_TEXT)
    } else if row.pending_new || row.pending_spent || row.local_pending_spent {
        rgb(theme::WARNING)
    } else {
        color
    }
}

pub(in crate::root) fn should_focus_utxo_table(
    active_activity: Activity,
    active_wallet_tab: WalletTab,
    state: Option<&ChainUtxoState>,
) -> bool {
    active_activity == Activity::Wallet
        && active_wallet_tab.shows_utxos()
        && state.is_some_and(ChainUtxoState::renders_table)
}

pub(in crate::root) fn should_refresh_utxo_ages(
    active_activity: Activity,
    active_wallet_tab: WalletTab,
    has_snapshot: bool,
) -> bool {
    active_activity == Activity::Wallet && active_wallet_tab.shows_utxos() && has_snapshot
}

pub(in crate::root) fn recoverable_poi_candidate_count(snapshot: &ListUtxosOutput) -> usize {
    snapshot
        .utxos
        .iter()
        .filter(|row| is_recoverable_poi_candidate(row))
        .count()
}

fn is_recoverable_poi_candidate(row: &UtxoOutput) -> bool {
    row.is_ppoi_retry_eligible()
}

pub(in crate::root) fn should_show_ppoi_retry_action(row: &UtxoDisplayRow) -> bool {
    !row.is_spent
        && !row.pending_new
        && !row.pending_spent
        && !row.local_pending_spent
        && row.activity_classification == "Private Output"
        && row.ppoi_state.retry_eligible()
}

pub(in crate::root) fn should_show_ppoi_submission_age(row: &UtxoDisplayRow) -> bool {
    row.ppoi_last_submission_at
        .is_some_and(|timestamp| timestamp != 0)
        && should_show_ppoi_retry_action(row)
}

pub(in crate::root) const fn global_poi_retry_available(
    session_available: bool,
    _refreshing: bool,
    workflow_needs_attention: u64,
    owned_retry_candidates: usize,
) -> bool {
    session_available && (owned_retry_candidates > 0 || workflow_needs_attention > 0)
}

pub(in crate::root) const fn poi_retry_button_label(refreshing: bool) -> &'static str {
    if refreshing {
        "Queue PPOI retry"
    } else {
        "Retry PPOI submissions"
    }
}

pub(in crate::root) const fn ppoi_row_retry_label(refreshing: bool) -> &'static str {
    if refreshing { "Queue retry" } else { "Retry" }
}

pub(in crate::root) const fn ppoi_state_detail(state: UtxoPpoiState) -> &'static str {
    match state {
        UtxoPpoiState::Valid => "Verified and spendable.",
        UtxoPpoiState::Missing => {
            "No proof has been submitted for this output yet. Retrying usually resolves it."
        }
        UtxoPpoiState::ProofSubmitted => "Submitted, awaiting verification.",
        UtxoPpoiState::Unknown => "Status not yet checked.",
        UtxoPpoiState::ShieldBlocked => "Blocked — use refund instead.",
        UtxoPpoiState::Mixed => "Verification lists disagree; not spendable while this is checked.",
    }
}

pub(in crate::root) fn ppoi_row_state_detail_with_submission(
    state: UtxoPpoiState,
    is_spent: bool,
    last_submission_at: Option<u64>,
) -> &'static str {
    if is_spent && matches!(state, UtxoPpoiState::Valid) {
        return "Verified — already spent.";
    }
    if last_submission_at.unwrap_or_default() != 0
        && matches!(
            state,
            UtxoPpoiState::Missing
                | UtxoPpoiState::Unknown
                | UtxoPpoiState::ProofSubmitted
                | UtxoPpoiState::Mixed
        )
    {
        return "Submission acknowledged; verification is pending.";
    }
    ppoi_state_detail(state)
}

pub(in crate::root) const fn ppoi_workflow_status_title(
    status: WalletPpoiWorkflowStatus,
    refreshing: bool,
) -> Option<&'static str> {
    let has_work = status.awaiting_submission > 0
        || status.awaiting_recovery > 0
        || status.awaiting_validation > 0
        || status.needs_attention > 0;
    if refreshing && has_work {
        Some("Recovering outgoing proofs…")
    } else if status.needs_attention > 0 || status.recovery_needs_attention > 0 {
        Some("PPOI submission needs attention")
    } else if status.awaiting_public_txid_data > 0 {
        Some("Waiting for public transaction proof data")
    } else if status.awaiting_poi_data > 0 {
        Some("Waiting for PPOI data")
    } else if status.retrying_recovery > 0 {
        Some("Outgoing proof recovery will retry")
    } else if status.awaiting_recovery > 0 || status.awaiting_submission > 0 {
        Some("Outgoing proof recovery pending")
    } else if status.awaiting_validation > 0 {
        Some("Awaiting PPOI verification")
    } else {
        None
    }
}

pub(in crate::root) fn ppoi_workflow_status_detail(status: WalletPpoiWorkflowStatus) -> String {
    let mut parts = Vec::with_capacity(8);
    let classified_recovery = status
        .awaiting_public_txid_data
        .saturating_add(status.awaiting_poi_data)
        .saturating_add(status.retrying_recovery)
        .saturating_add(status.recovery_needs_attention)
        .min(status.awaiting_recovery);
    let unclassified_recovery = status.awaiting_recovery.saturating_sub(classified_recovery);
    if status.awaiting_public_txid_data > 0 {
        parts.push(ppoi_workflow_count_label(
            status.awaiting_public_txid_data,
            "waiting for public transaction data",
        ));
    }
    if status.awaiting_poi_data > 0 {
        parts.push(ppoi_workflow_count_label(
            status.awaiting_poi_data,
            "waiting for PPOI data",
        ));
    }
    if status.retrying_recovery > 0 {
        parts.push(ppoi_workflow_count_label(
            status.retrying_recovery,
            "retrying recovery",
        ));
    }
    if status.recovery_needs_attention > 0 {
        parts.push(ppoi_workflow_count_label(
            status.recovery_needs_attention,
            "recovery needing attention",
        ));
    }
    if unclassified_recovery > 0 {
        parts.push(ppoi_workflow_count_label(
            unclassified_recovery,
            "awaiting recovery",
        ));
    }
    if status.awaiting_submission > 0 {
        parts.push(ppoi_workflow_count_label(
            status.awaiting_submission,
            "awaiting submission",
        ));
    }
    if status.awaiting_validation > 0 {
        parts.push(ppoi_workflow_count_label(
            status.awaiting_validation,
            "awaiting verification",
        ));
    }
    if status.needs_attention > 0 {
        parts.push(ppoi_workflow_count_label(
            status.needs_attention,
            "needs attention",
        ));
    }
    if parts.is_empty() {
        "Checking proofs from the sending wallet.".to_string()
    } else {
        parts.join(" · ")
    }
}

fn ppoi_workflow_count_label(count: u64, suffix: &str) -> String {
    let noun = if count == 1 { "PPOI" } else { "PPOIs" };
    format!("{count} {noun} {suffix}")
}

pub(in crate::root) const fn ppoi_retry_tooltip(state: UtxoPpoiState) -> &'static str {
    ppoi_state_detail(state)
}
