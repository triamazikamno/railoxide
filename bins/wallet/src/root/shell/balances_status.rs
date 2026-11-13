use std::time::Duration;

use gpui::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::progress::Progress as UiProgress;
use ui::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::{PublicScanSource, WalletNetworkMode, WalletPpoiWorkflowStatus, WalletSyncTip};

use super::super::chain_load::{
    BalanceSyncIssue, PresenceStatus, SyncStatusContext, SyncStatusLabels, WalletStatusCounts,
    balance_sync_issue, sync_status_labels,
};
use super::super::submission_route::{presence_status_color, status_presence_dot};
use super::super::{ChainUtxoState, WalletRoot, count_label, rgb_with_alpha};
use super::{now_epoch_secs, render_status_hover_note_base};

pub(super) fn render_balances_status_hover_target(
    root: &Entity<WalletRoot>,
    id: &'static str,
) -> gpui::Stateful<gpui::Div> {
    let tooltip_root = root.clone();
    div().id(id).hoverable_tooltip(move |_window, cx| {
        let root = tooltip_root.clone();
        cx.new(|cx| BalancesStatusHoverCard::new(root, cx)).into()
    })
}

struct BalancesStatusHoverCard {
    root: Entity<WalletRoot>,
}

impl BalancesStatusHoverCard {
    fn new(root: Entity<WalletRoot>, cx: &mut Context<'_, Self>) -> Self {
        cx.observe(&root, |_this, _root, cx| cx.notify()).detach();
        Self { root }
    }
}

impl Render for BalancesStatusHoverCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let now = now_epoch_secs();
        let (status, labels, sync_tip, data_source, issue, counts, network_mode) = {
            let root = self.root.read(cx);
            let chain_id = root.last_evm_chain;
            let block_time = root
                .effective_chain_configs
                .get(chain_id)
                .and_then(|chain| chain.block_time);
            let state = root.chain_states.get(&chain_id);
            let counts = root.wallet_status_counts(
                state.and_then(ChainUtxoState::snapshot).map(AsRef::as_ref),
                state.map_or_else(
                    WalletPpoiWorkflowStatus::default,
                    ChainUtxoState::ppoi_workflow_status,
                ),
            );
            let context = state.and_then(|state| match state {
                ChainUtxoState::Loading { .. } => Some(SyncStatusContext::Loading),
                ChainUtxoState::Syncing { .. } => Some(SyncStatusContext::Syncing),
                ChainUtxoState::Idle
                | ChainUtxoState::Ready { .. }
                | ChainUtxoState::Error { .. } => None,
            });
            let progress = state.and_then(ChainUtxoState::progress);
            let labels = context.map(|context| sync_status_labels(context, progress));
            let status = state.map_or(PresenceStatus::Unknown, |state| {
                root.balances_status_for_state(state)
            });
            let sync_tip = state.and_then(ChainUtxoState::sync_tip);
            let data_source = balance_sync_data_source(context, progress);
            let issue = state
                .filter(|state| matches!(state, ChainUtxoState::Ready { .. }))
                .and_then(|_| block_time.and_then(|time| balance_sync_issue(sync_tip, time, now)));
            (
                status,
                labels,
                sync_tip,
                data_source,
                issue,
                counts,
                root.http.network_mode(),
            )
        };
        let color = presence_status_color(status);

        div()
            // Presses on the card must not reach a dialog backdrop beneath it.
            .occlude()
            .w(px(360.0))
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap_3()
            .text_size(APP_TEXT_SIZE)
            .text_color(rgb(theme::TEXT))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(status_presence_dot(status).flex_none())
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(color))
                            .child(balances_hover_heading(status, labels.as_ref(), issue)),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(18.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(balances_hover_detail(
                        status,
                        labels.as_ref(),
                        issue,
                        network_mode,
                    )),
            )
            .when_some(labels.as_ref(), |this, labels| {
                this.child(render_balance_sync_progress_section(labels, data_source))
            })
            .when_some(sync_tip, |this, sync_tip| {
                this.child(render_balance_sync_tip_section(sync_tip, now))
            })
            .when_some(balance_pending_detail(counts), |this, detail| {
                this.child(render_status_hover_note_base(
                    "Balance updates pending",
                    &detail,
                    theme::WARNING,
                    0.08,
                ))
            })
    }
}

fn balances_hover_heading(
    status: PresenceStatus,
    labels: Option<&SyncStatusLabels>,
    issue: Option<BalanceSyncIssue>,
) -> String {
    if let Some(issue) = issue {
        return balance_sync_issue_heading(issue).to_string();
    }
    if let Some(labels) = labels {
        return labels.title.clone();
    }
    match status {
        PresenceStatus::Healthy => "Balances ready",
        PresenceStatus::Active => "Balances catching up",
        PresenceStatus::Error => "Balance sync error",
        PresenceStatus::Unknown => "Balances unavailable",
    }
    .to_string()
}

fn balances_hover_detail(
    status: PresenceStatus,
    labels: Option<&SyncStatusLabels>,
    issue: Option<BalanceSyncIssue>,
    network_mode: WalletNetworkMode,
) -> String {
    if let Some(issue) = issue {
        return balance_sync_issue_detail(issue, network_mode);
    }
    if labels.is_some() {
        return "Private balance sync is catching up with chain state.".to_string();
    }
    match status {
        PresenceStatus::Healthy => "Private balances are synced and following chain state.",
        PresenceStatus::Active => "Private balance sync is catching up with chain state.",
        PresenceStatus::Error => "Private balance sync reported an error.",
        PresenceStatus::Unknown => "Private balance sync state is not available yet.",
    }
    .to_string()
}

fn render_balance_sync_progress_section(
    labels: &SyncStatusLabels,
    data_source: Option<PublicScanSource>,
) -> gpui::Div {
    div()
        .rounded_md()
        .border_1()
        .border_color(rgb_with_alpha(theme::WARNING, 0.34))
        .bg(rgb_with_alpha(theme::WARNING, 0.05))
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    UiProgress::new("balance-sync-progress")
                        .flex_1()
                        .h(px(7.0))
                        .value(f32::from(labels.percent))
                        .bg(rgb(theme::WARNING)),
                )
                .child(
                    div()
                        .w(px(42.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::WARNING))
                        .child(format!("{}%", labels.percent)),
                ),
        )
        .child(
            div()
                .text_size(px(12.0))
                .line_height(px(17.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(labels.detail.clone()),
        )
        .when_some(data_source, |section, source| {
            section.child(render_balance_sync_tip_row(
                "Data source",
                balance_sync_source_label(source).to_string(),
            ))
        })
}

pub(in crate::root) const fn balance_sync_source_label(source: PublicScanSource) -> &'static str {
    match source {
        PublicScanSource::CachedCoverage => "Local cache",
        PublicScanSource::IndexedArtifacts => "Verified artifacts",
        PublicScanSource::Squid => "Squid index",
        PublicScanSource::Rpc => "RPC",
        PublicScanSource::ArchiveRpc => "Archive RPC",
    }
}

pub(in crate::root) const fn balance_sync_data_source(
    context: Option<SyncStatusContext>,
    progress: Option<wallet_ops::SyncProgressUpdate>,
) -> Option<PublicScanSource> {
    match (context, progress) {
        (Some(_), Some(progress)) => progress.source,
        _ => None,
    }
}

fn render_balance_sync_tip_section(sync_tip: WalletSyncTip, now_secs: u64) -> gpui::Div {
    div()
        .rounded_md()
        .border_1()
        .border_color(rgb_with_alpha(theme::BORDER, 0.72))
        .bg(rgb_with_alpha(theme::SURFACE_ELEVATED, 0.34))
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(theme::TEXT))
                .child("Chain position"),
        )
        .child(render_balance_sync_tip_row(
            "Wallet state",
            format_block_label(sync_tip.last_scanned_block),
        ))
        .child(render_balance_sync_tip_row(
            "Safe head",
            format_block_label(sync_tip.safe_head_block),
        ))
        .child(render_balance_sync_tip_row(
            "RPC head",
            format_block_label(sync_tip.head_block),
        ))
        .when_some(
            sync_tip.head_last_advanced_at_unix_secs,
            |this, advanced_at| {
                this.child(render_balance_sync_tip_row(
                    "Head advanced",
                    ui::format::format_relative_age(Duration::from_secs(
                        now_secs.saturating_sub(advanced_at),
                    )),
                ))
            },
        )
}

fn render_balance_sync_tip_row(label: &'static str, value: String) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .text_size(px(12.0))
        .child(
            div()
                .min_w_0()
                .text_color(rgb(theme::TEXT_MUTED))
                .truncate()
                .child(label),
        )
        .child(
            div()
                .flex_none()
                .font_family(APP_MONO_FONT_FAMILY)
                .text_color(rgb(theme::TEXT))
                .child(value),
        )
}

const fn balance_sync_issue_heading(issue: BalanceSyncIssue) -> &'static str {
    match issue {
        BalanceSyncIssue::HeadUnavailable => "Balance head unavailable",
        BalanceSyncIssue::HeadStalled { .. } => "Balance source stale",
        BalanceSyncIssue::Lagging { .. } => "Balances lagging",
    }
}

pub(in crate::root) fn balance_sync_issue_detail(
    issue: BalanceSyncIssue,
    network_mode: WalletNetworkMode,
) -> String {
    match issue {
        BalanceSyncIssue::HeadUnavailable => "Waiting for chain head updates.".to_string(),
        BalanceSyncIssue::HeadStalled {
            stale_secs,
            threshold_secs: _,
        } => format!(
            "RPC head has not advanced for {}. {}",
            ui::format::format_compact_duration(Duration::from_secs(stale_secs)),
            balance_sync_issue_suggestion(network_mode)
        ),
        BalanceSyncIssue::Lagging {
            lag_blocks,
            threshold_blocks: _,
        } => format!(
            "Wallet state is {lag_blocks} safe-head blocks behind. {}",
            balance_sync_issue_suggestion(network_mode)
        ),
    }
}

const fn balance_sync_issue_suggestion(network_mode: WalletNetworkMode) -> &'static str {
    match network_mode {
        WalletNetworkMode::Tor => "Consider generating a new Tor session or using premium RPCs.",
        WalletNetworkMode::Proxy | WalletNetworkMode::Direct => "Consider using premium RPCs.",
    }
}

fn balance_pending_detail(counts: WalletStatusCounts) -> Option<String> {
    let mut parts = Vec::new();
    if counts.pending_incoming_outputs > 0 {
        parts.push(count_label(
            counts.pending_incoming_outputs,
            "incoming output",
        ));
    }
    if counts.pending_outgoing_outputs > 0 {
        parts.push(count_label(
            counts.pending_outgoing_outputs,
            "outgoing output",
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!(
            "{} waiting for confirmation and safe-head finality.",
            parts.join(" · ")
        ))
    }
}

fn format_block_label(block: Option<u64>) -> String {
    block.map_or_else(|| "Waiting".to_string(), |block| format!("block {block}"))
}
