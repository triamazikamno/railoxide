use std::time::Duration;

use gpui::{ParentElement, SharedString, Styled, div, prelude::FluentBuilder as _, px, rgb};
use gpui_component::progress::Progress as UiProgress;
use ui::theme::{self, APP_TEXT_SIZE};
use wallet_ops::{
    PoiArtifactCacheProgress, SyncProgressStage, SyncProgressUnit, SyncProgressUpdate,
    WalletPpoiWorkflowStatus, WalletSyncTip,
};

use super::super::count_label;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum SyncStatusContext {
    Loading,
    Syncing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct SyncStatusLabels {
    pub(in crate::root) title: String,
    pub(in crate::root) percent: u8,
    pub(in crate::root) detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum PresenceStatus {
    Healthy,
    Active,
    Error,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum BalanceSyncIssue {
    HeadUnavailable,
    HeadStalled {
        stale_secs: u64,
        threshold_secs: u64,
    },
    Lagging {
        lag_blocks: u64,
        threshold_blocks: u64,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::root) struct WalletStatusCounts {
    pub(in crate::root) pending_incoming_outputs: usize,
    pub(in crate::root) pending_outgoing_outputs: usize,
    pub(in crate::root) pending_poi_assets: usize,
    pub(in crate::root) recoverable_poi_outputs: usize,
    pub(in crate::root) blocked_shield_outputs: usize,
    pub(in crate::root) ppoi_workflow_status: WalletPpoiWorkflowStatus,
}

impl WalletStatusCounts {
    pub(in crate::root) fn ppoi_attention_count(self) -> usize {
        let workflow_attention = usize::try_from(
            self.ppoi_workflow_status
                .needs_attention
                .saturating_add(self.ppoi_workflow_status.recovery_needs_attention),
        )
        .unwrap_or(usize::MAX);
        self.recoverable_poi_outputs
            .saturating_add(workflow_attention)
            .saturating_add(self.blocked_shield_outputs)
    }

    pub(in crate::root) fn ppoi_status_count(self) -> usize {
        let workflow_count =
            usize::try_from(self.ppoi_workflow_status.outstanding_count()).unwrap_or(usize::MAX);
        workflow_count
            .saturating_add(self.recoverable_poi_outputs)
            .saturating_add(self.blocked_shield_outputs)
    }

    pub(in crate::root) const fn has_ppoi_blocking_checks(self) -> bool {
        self.pending_poi_assets > 0
            || self.recoverable_poi_outputs > 0
            || self.ppoi_workflow_status.has_outstanding()
    }
}

pub(in crate::root) fn balances_presence_status(
    syncing: bool,
    ready: bool,
    sync_tip: Option<WalletSyncTip>,
    block_time: Duration,
    now_secs: u64,
) -> PresenceStatus {
    if syncing {
        return PresenceStatus::Active;
    }
    if !ready {
        return PresenceStatus::Unknown;
    }
    if sync_tip.is_some_and(|tip| tip.indexed_catch_up.is_some()) {
        return PresenceStatus::Active;
    }
    match balance_sync_issue(sync_tip, block_time, now_secs) {
        None => PresenceStatus::Healthy,
        Some(BalanceSyncIssue::HeadUnavailable) => PresenceStatus::Unknown,
        Some(BalanceSyncIssue::HeadStalled { .. } | BalanceSyncIssue::Lagging { .. }) => {
            PresenceStatus::Active
        }
    }
}

pub(in crate::root) fn balance_sync_issue(
    sync_tip: Option<WalletSyncTip>,
    block_time: Duration,
    now_secs: u64,
) -> Option<BalanceSyncIssue> {
    let Some(sync_tip) = sync_tip else {
        return Some(BalanceSyncIssue::HeadUnavailable);
    };
    if sync_tip.head_block.is_none() {
        return Some(BalanceSyncIssue::HeadUnavailable);
    }
    let Some(safe_head_block) = sync_tip.safe_head_block else {
        return Some(BalanceSyncIssue::HeadUnavailable);
    };
    let Some(head_last_advanced_at) = sync_tip.head_last_advanced_at_unix_secs else {
        return Some(BalanceSyncIssue::HeadUnavailable);
    };

    let threshold_secs = balance_stale_timeout(block_time).as_secs();
    let stale_secs = now_secs.saturating_sub(head_last_advanced_at);
    if stale_secs > threshold_secs {
        return Some(BalanceSyncIssue::HeadStalled {
            stale_secs,
            threshold_secs,
        });
    }

    let Some(last_scanned_block) = sync_tip.last_scanned_block else {
        return Some(BalanceSyncIssue::HeadUnavailable);
    };
    let lag_blocks = safe_head_block.saturating_sub(last_scanned_block);
    let threshold_blocks = balance_lag_threshold_blocks(block_time);
    if lag_blocks > threshold_blocks {
        return Some(BalanceSyncIssue::Lagging {
            lag_blocks,
            threshold_blocks,
        });
    }

    None
}

pub(in crate::root) fn balance_stale_timeout(block_time: Duration) -> Duration {
    block_time.saturating_mul(10).max(Duration::from_secs(45))
}

pub(in crate::root) fn balance_lag_threshold_blocks(block_time: Duration) -> u64 {
    let threshold = balance_stale_timeout(block_time).as_nanos() / block_time.as_nanos().max(1);
    u64::try_from(threshold).unwrap_or(u64::MAX).max(2)
}

pub(in crate::root) fn ppoi_presence_status(
    refreshing: bool,
    source_available: bool,
    artifact_cache_expected: bool,
    artifact_progress: Option<&PoiArtifactCacheProgress>,
    counts: WalletStatusCounts,
) -> PresenceStatus {
    if !source_available {
        return PresenceStatus::Unknown;
    }

    if let Some(progress) = artifact_progress {
        if progress.is_error() {
            return if !progress.ready_for_wallet_checks && counts.has_ppoi_blocking_checks() {
                PresenceStatus::Error
            } else {
                PresenceStatus::Active
            };
        }
        if progress.is_active() {
            return PresenceStatus::Active;
        }
        if !progress.is_ready() {
            return PresenceStatus::Unknown;
        }
    } else if artifact_cache_expected {
        return if refreshing {
            PresenceStatus::Active
        } else {
            PresenceStatus::Unknown
        };
    }

    if refreshing {
        PresenceStatus::Active
    } else if counts.blocked_shield_outputs > 0
        || counts.ppoi_workflow_status.needs_attention > 0
        || counts.ppoi_workflow_status.recovery_needs_attention > 0
    {
        PresenceStatus::Error
    } else if counts.ppoi_status_count() > 0 {
        PresenceStatus::Active
    } else {
        PresenceStatus::Healthy
    }
}

pub(in crate::root) fn ready_wallet_status_labels(counts: WalletStatusCounts) -> SyncStatusLabels {
    let title = if counts.blocked_shield_outputs > 0 {
        "Private assets need attention"
    } else if counts.recoverable_poi_outputs > 0
        || counts.pending_poi_assets > 0
        || counts.pending_incoming_outputs > 0
    {
        "Not yet spendable"
    } else if counts.pending_outgoing_outputs > 0 {
        "Waiting for confirmation"
    } else {
        "Wallet ready"
    };
    SyncStatusLabels {
        title: title.to_string(),
        percent: 100,
        detail: ready_wallet_status_detail(counts),
    }
}

fn ready_wallet_status_detail(counts: WalletStatusCounts) -> String {
    if counts.blocked_shield_outputs > 0 {
        let verb = if counts.blocked_shield_outputs == 1 {
            " needs attention"
        } else {
            " need attention"
        };
        return count_label(counts.blocked_shield_outputs, "blocked Shield") + verb;
    }
    if counts.recoverable_poi_outputs > 0 {
        return if counts.recoverable_poi_outputs == 1 {
            "1 PPOI verification retry is available".to_string()
        } else {
            format!(
                "{} PPOI verification retries are available",
                counts.recoverable_poi_outputs
            )
        };
    }
    let mut parts = Vec::new();
    if counts.pending_incoming_outputs > 0 {
        parts.push(count_label(
            counts.pending_incoming_outputs,
            "incoming transfer",
        ));
    }
    if counts.pending_outgoing_outputs > 0 {
        parts.push(count_label(
            counts.pending_outgoing_outputs,
            "outgoing transfer",
        ));
    }
    if counts.pending_poi_assets > 0 {
        parts.push(format!(
            "{} awaiting PPOI verification",
            count_label(counts.pending_poi_assets, "asset")
        ));
    }
    if parts.is_empty() {
        "Private wallet synced and ready".to_string()
    } else {
        parts.join(" · ")
    }
}

impl SyncStatusContext {
    const fn fallback_title(self) -> &'static str {
        match self {
            Self::Loading => "Preparing wallet sync",
            Self::Syncing => "Checking wallet sync",
        }
    }

    const fn fallback_detail(self) -> &'static str {
        match self {
            Self::Loading => "Connecting to chain and loading local wallet state...",
            Self::Syncing => "Checking for new wallet events...",
        }
    }
}

pub(in crate::root) fn loading_summary(progress: Option<SyncProgressUpdate>) -> String {
    progress.map_or_else(
        || "Preparing wallet sync...".to_string(),
        |progress| format!("{} · {}%", progress.stage.label(), progress.percent()),
    )
}

pub(in crate::root) fn sync_status_labels(
    context: SyncStatusContext,
    progress: Option<SyncProgressUpdate>,
) -> SyncStatusLabels {
    let progress = match context {
        SyncStatusContext::Loading => progress,
        SyncStatusContext::Syncing => syncing_progress(progress),
    };
    SyncStatusLabels {
        title: progress.map_or_else(
            || context.fallback_title().to_string(),
            |progress| progress.stage.label().to_string(),
        ),
        percent: progress.map_or(0, SyncProgressUpdate::percent),
        detail: progress.map_or_else(|| context.fallback_detail().to_string(), progress_detail),
    }
}

pub(super) const fn syncing_progress(
    progress: Option<SyncProgressUpdate>,
) -> Option<SyncProgressUpdate> {
    match progress {
        Some(progress) if matches!(progress.stage, SyncProgressStage::SynchronizingCommitments) => {
            None
        }
        progress => progress,
    }
}

pub(in crate::root) fn sync_status_center(
    context: SyncStatusContext,
    progress: Option<SyncProgressUpdate>,
) -> gpui::Div {
    wallet_status_center(sync_status_labels(context, progress), true)
}

pub(in crate::root) fn ready_status_center(counts: WalletStatusCounts) -> Option<gpui::Div> {
    ready_wallet_status_shows_text(counts)
        .then(|| wallet_status_center(ready_wallet_status_labels(counts), false))
}

pub(in crate::root) const fn ready_wallet_status_shows_text(_counts: WalletStatusCounts) -> bool {
    false
}

/// Sync progress for the status bar center. Only the detail shrinks, so it truncates first.
fn wallet_status_center(labels: SyncStatusLabels, show_progress: bool) -> gpui::Div {
    // Sized to its content so the status bar's center region can center it; it still shrinks.
    div()
        .min_w(px(0.0))
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .flex_none()
                .text_color(rgb(theme::TEXT))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(SharedString::from(labels.title)),
        )
        .when(show_progress, |bar| {
            bar.child(
                UiProgress::new("chain-load-progress")
                    .flex_none()
                    .w(px(190.0))
                    .h(px(6.0))
                    .value(f32::from(labels.percent)),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(42.0))
                    .text_color(rgb(theme::INFO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(SharedString::from(format!("{}%", labels.percent))),
            )
        })
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_color(rgb(theme::TEXT_MUTED))
                .text_size(APP_TEXT_SIZE)
                .map(|detail| {
                    if show_progress {
                        // The id keeps one animation as the detail text changes.
                        detail
                            .child(ui::controls::app_shimmer(labels.detail).id("chain-load-detail"))
                    } else {
                        detail.child(SharedString::from(labels.detail))
                    }
                }),
        )
}

pub(in crate::root) fn progress_detail(progress: SyncProgressUpdate) -> String {
    match progress.unit {
        SyncProgressUnit::Block => {}
        SyncProgressUnit::ArtifactPreparation => {
            return "Preparing artifact metadata...".to_string();
        }
        SyncProgressUnit::ArtifactChunk { completed, total } => {
            if total == 0 {
                return "Artifact chunks prepared".to_string();
            }
            if completed == 0 {
                return format!("Downloading {total} artifact chunks...");
            }
            let completed = completed.min(total);
            if completed == total {
                return "Artifact chunks ready".to_string();
            }
            if completed.saturating_add(1) == total {
                return format!("Fetching final artifact chunk ({completed} of {total} ready)...");
            }
            return format!("Artifact chunks ready: {completed} of {total}");
        }
        SyncProgressUnit::ArtifactApplied => {
            return match progress.stage {
                wallet_ops::SyncProgressStage::SynchronizingCommitments => {
                    "Commitment artifacts applied".to_string()
                }
                wallet_ops::SyncProgressStage::PreparingUtxoIndex => {
                    "UTXO index artifacts prepared".to_string()
                }
                wallet_ops::SyncProgressStage::IndexingUtxos => "Artifacts applied".to_string(),
            };
        }
        SyncProgressUnit::CommitmentTail => {
            let current = progress
                .current_block
                .max(progress.start_block)
                .min(progress.target_block);
            return format!(
                "Checking commitment tail: block {current} of {}",
                progress.target_block
            );
        }
    }
    let current = progress
        .current_block
        .max(progress.start_block)
        .min(progress.target_block);
    format!("Block {current} of {}", progress.target_block)
}
