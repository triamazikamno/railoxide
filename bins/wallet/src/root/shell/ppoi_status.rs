use std::fmt::Write as _;

use alloy::hex;
use gpui::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{Disableable, Sizable, WindowExt, progress::Progress as UiProgress};
use ui::controls::app_button;
use ui::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::{
    PoiArtifactCacheFailureKind, PoiArtifactCacheListProgress, PoiArtifactCachePhase,
    PoiArtifactCacheProgress, WalletPpoiWorkflowStatus,
};

use super::super::chain_load::{PresenceStatus, WalletStatusCounts};
use super::super::submission_route::{presence_status_color, status_presence_dot};
use super::super::ui_helpers::format_binary_bytes;
use super::super::utxo::{ppoi_workflow_status_detail, ppoi_workflow_status_title};
use super::super::{ChainUtxoState, WalletRoot, count_label, rgb_with_alpha};
use super::render_status_hover_note_base;

pub(super) fn render_ppoi_status_hover_target(
    root: &Entity<WalletRoot>,
    id: &'static str,
) -> gpui::Stateful<gpui::Div> {
    let tooltip_root = root.clone();
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .hoverable_tooltip(move |_window, cx| {
            let root = tooltip_root.clone();
            cx.new(|cx| PpoiStatusHoverCard::new(root, cx)).into()
        })
}

struct PpoiStatusHoverCard {
    root: Entity<WalletRoot>,
}

impl PpoiStatusHoverCard {
    fn new(root: Entity<WalletRoot>, cx: &mut Context<'_, Self>) -> Self {
        cx.observe(&root, |_this, _root, cx| cx.notify()).detach();
        Self { root }
    }
}

impl Render for PpoiStatusHoverCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let (status, progress, refreshing, counts, retrying) = {
            let root = self.root.read(cx);
            let state = root.chain_states.get(&root.last_evm_chain);
            let counts = root.wallet_status_counts(
                state.and_then(ChainUtxoState::snapshot).map(AsRef::as_ref),
                state.map_or_else(
                    WalletPpoiWorkflowStatus::default,
                    ChainUtxoState::ppoi_workflow_status,
                ),
            );
            let status = state.map_or(PresenceStatus::Unknown, |state| {
                root.ppoi_status_for_state(state, counts)
            });
            let refreshing = state.is_some_and(ChainUtxoState::poi_refreshing);
            let progress = root.selected_chain_poi_artifact_progress().cloned();
            let retrying = root
                .poi_artifact_cache_retry_attempts
                .contains(root.last_evm_chain);
            (status, progress, refreshing, counts, retrying)
        };
        let color = presence_status_color(status);
        let event_label = ppoi_event_header_label(progress.as_ref());
        let dialog_open = window.has_active_dialog(cx);

        div()
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
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(color))
                                    .truncate()
                                    .child(ppoi_hover_heading(
                                        status,
                                        progress.as_ref(),
                                        refreshing,
                                    )),
                            )
                            .when_some(event_label, |this, label| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .text_color(rgb(theme::TEXT_MUTED))
                                        .child(format!("({label})")),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(18.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .when_some(
                        ppoi_hover_detail(status, progress.as_ref(), refreshing),
                        gpui::ParentElement::child,
                    ),
            )
            .when_some(
                progress
                    .as_ref()
                    .filter(|progress| progress.total_lists > 1),
                |this, progress| this.child(render_ppoi_list_progress_section(progress)),
            )
            .when_some(
                progress.as_ref().filter(|progress| !progress.is_ready()),
                |this, progress| {
                    if progress.is_error() {
                        this.child(render_ppoi_artifact_error_section(
                            self.root.clone(),
                            progress,
                            status,
                            retrying,
                        ))
                    } else {
                        this.child(render_ppoi_artifact_progress_section(progress, status))
                    }
                },
            )
            .when(refreshing, |this| {
                this.child(render_ppoi_hover_note(
                    "Submitting PPOIs…",
                    "Submitting sender-created contexts and checking owned outputs. Open Private asset status to review or retry blocked work.",
                    theme::WARNING,
                ))
            })
            .when(
                counts.ppoi_workflow_status.has_outstanding() && !refreshing,
                |this| {
                    this.child(render_ppoi_hover_note(
                        ppoi_workflow_status_title(counts.ppoi_workflow_status, false)
                            .unwrap_or("Outgoing proof recovery"),
                        &ppoi_workflow_status_detail(counts.ppoi_workflow_status),
                        if counts.ppoi_workflow_status.needs_attention > 0
                            || counts.ppoi_workflow_status.recovery_needs_attention > 0
                        {
                            theme::DANGER
                        } else {
                            theme::WARNING
                        },
                    ))
                },
            )
            .when(counts.ppoi_attention_count() > 0, |this| {
                let detail = ppoi_attention_detail(counts);
                let color = ppoi_attention_hover_color(counts);
                // Above a dialog the note only informs; its action would close the dialog.
                if dialog_open {
                    this.child(render_ppoi_hover_note("Needs review", &detail, color))
                } else {
                    this.child(render_ppoi_hover_action_note(
                        self.root.clone(),
                        "Needs review",
                        &detail,
                        color,
                    ))
                }
            })
    }
}

fn render_ppoi_artifact_progress_section(
    progress: &PoiArtifactCacheProgress,
    status: PresenceStatus,
) -> gpui::Div {
    let percent = progress.percent();
    let completed_lists = if percent == 100 && progress.total_lists > 0 {
        progress.total_lists
    } else {
        progress.completed_lists.min(progress.total_lists)
    };
    let list_count = if progress.total_lists == 1 {
        "list"
    } else {
        "lists"
    };
    let list_text = if progress.total_lists == 0 {
        "Preparing POI list metadata".to_string()
    } else if progress.total_lists == 1 && completed_lists == 1 {
        "POI list ready".to_string()
    } else {
        format!(
            "{} of {} {} ready",
            completed_lists, progress.total_lists, list_count
        )
    };
    let color = presence_status_color(status);

    div()
        .rounded_md()
        .border_1()
        .border_color(rgb_with_alpha(color, 0.24))
        .bg(rgb_with_alpha(color, 0.05))
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(color))
                        .child(ppoi_artifact_phase_label(progress.phase)),
                )
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(color))
                        .child(format!("{percent}%")),
                ),
        )
        .child(
            UiProgress::new("ppoi-artifact-progress")
                .h(px(7.0))
                .value(f32::from(percent))
                .bg(rgb(color)),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(list_text),
        )
        .when_some(
            ppoi_chunk_progress_label(progress),
            |this, chunk_progress| {
                this.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(chunk_progress),
                )
            },
        )
        .when_some(
            ppoi_replay_progress_label(progress),
            |this, replay_progress| {
                this.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(replay_progress),
                )
            },
        )
        .when(
            progress.current_event_index.is_some() || progress.target_event_index.is_some(),
            |this| {
                this.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(ppoi_event_progress_label(progress)),
                )
            },
        )
        .when_some(progress.current_list_key.as_ref(), |this, list_key| {
            this.child(
                div()
                    .font_family(APP_MONO_FONT_FAMILY)
                    .text_size(px(12.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(format!("List {}", short_poi_list_key(list_key.as_slice()))),
            )
        })
        .when_some(progress.last_error.as_ref(), |this, error| {
            this.child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(17.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(format!("Last error: {error}")),
            )
        })
}

fn render_ppoi_artifact_error_section(
    root: Entity<WalletRoot>,
    progress: &PoiArtifactCacheProgress,
    status: PresenceStatus,
    retrying: bool,
) -> gpui::Div {
    let color = if status == PresenceStatus::Error {
        theme::DANGER
    } else {
        theme::WARNING
    };
    let error = progress
        .last_error
        .clone()
        .unwrap_or_else(|| "Artifact cache refresh failed.".to_string());

    div()
        .rounded_md()
        .border_1()
        .border_color(rgb_with_alpha(color, 0.34))
        .bg(rgb_with_alpha(color, 0.05))
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(color))
                .child("Last refresh failed"),
        )
        .child(
            div()
                .text_size(px(12.0))
                .line_height(px(17.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(error),
        )
        .child(
            div().flex().justify_end().child(
                app_button("wallet-status-ppoi-retry-artifact-cache", "Retry refresh")
                    .small()
                    .loading(retrying)
                    .disabled(retrying)
                    .on_click(move |_event, _window, cx| {
                        cx.stop_propagation();
                        root.update(cx, |root, cx| {
                            root.retry_selected_poi_artifact_cache_refresh(cx);
                        });
                    }),
            ),
        )
}

fn render_ppoi_hover_note(title: &str, detail: &str, color: u32) -> gpui::Div {
    render_ppoi_hover_note_base(title, detail, color, 0.08)
}

fn render_ppoi_hover_action_note(
    root: Entity<WalletRoot>,
    title: &'static str,
    detail: &str,
    color: u32,
) -> gpui::Stateful<gpui::Div> {
    render_ppoi_hover_note_base(title, detail, color, 0.08)
        .id("wallet-status-ppoi-needs-review")
        .cursor_pointer()
        .hover(move |this| this.bg(rgb_with_alpha(color, 0.14)))
        .on_click(move |_event, window, cx| {
            cx.stop_propagation();
            root.update(cx, |root, cx| {
                root.open_private_pending_status_dialog(window, cx);
            });
        })
}

fn render_ppoi_hover_note_base(title: &str, detail: &str, color: u32, bg_alpha: f32) -> gpui::Div {
    render_status_hover_note_base(title, detail, color, bg_alpha)
}

pub(in crate::root) fn ppoi_hover_heading(
    status: PresenceStatus,
    progress: Option<&PoiArtifactCacheProgress>,
    refreshing: bool,
) -> &'static str {
    if let Some(progress) = progress {
        if progress.is_error() {
            return match progress.failure_kind() {
                Some(PoiArtifactCacheFailureKind::ServingCorpusUnavailable) => {
                    "PPOI data unavailable"
                }
                None if status == PresenceStatus::Error => "PPOI checks blocked",
                Some(PoiArtifactCacheFailureKind::RefreshDegraded) | None => {
                    "Artifact cache refresh failed"
                }
            };
        }
        if progress.is_active() {
            if progress.ready_for_wallet_checks {
                return "Refreshing PPOI data";
            }
            return match progress.phase {
                PoiArtifactCachePhase::LoadingPersisted => "Loading saved PPOI data",
                PoiArtifactCachePhase::Resetting => "Resetting PPOI data",
                PoiArtifactCachePhase::ResolvingManifest => "Resolving POI manifest",
                PoiArtifactCachePhase::VerifyingCatalog => "Verifying POI catalog",
                PoiArtifactCachePhase::Planning => "Planning POI refresh",
                PoiArtifactCachePhase::DownloadingChunks => "Downloading POI chunks",
                PoiArtifactCachePhase::ReplayingRanges => "Replaying POI event ranges",
                PoiArtifactCachePhase::Validating => "Validating PPOI data",
                PoiArtifactCachePhase::Persisting => "Saving PPOI data",
                PoiArtifactCachePhase::LiveTailing => "Following POI event tail",
                PoiArtifactCachePhase::Idle
                | PoiArtifactCachePhase::Ready
                | PoiArtifactCachePhase::Failed => "PPOI catching up",
            };
        }
    }
    if refreshing {
        return "Submitting PPOIs…";
    }
    match status {
        PresenceStatus::Healthy => "PPOI ready",
        PresenceStatus::Active => "PPOI action needed",
        PresenceStatus::Error => "PPOI checks blocked",
        PresenceStatus::Unknown => "PPOI status unavailable",
    }
}

pub(in crate::root) fn ppoi_hover_detail(
    status: PresenceStatus,
    progress: Option<&PoiArtifactCacheProgress>,
    refreshing: bool,
) -> Option<&'static str> {
    if let Some(progress) = progress {
        if progress.is_error() {
            return match progress.failure_kind() {
                Some(PoiArtifactCacheFailureKind::RefreshDegraded) => Some(
                    "Refresh failed; wallet checks continue using verified PPOI data saved on this device.",
                ),
                Some(PoiArtifactCacheFailureKind::ServingCorpusUnavailable) => {
                    Some("No verified PPOI data is available for wallet checks.")
                }
                None if progress.ready_for_wallet_checks => Some(
                    "Refresh failed; wallet checks continue using verified PPOI data saved on this device.",
                ),
                None => Some("No verified PPOI data is available for wallet checks."),
            };
        }
        if progress.is_active() {
            return progress.ready_for_wallet_checks.then_some(
                "Refreshing in the background while wallet checks use verified PPOI data saved on this device.",
            );
        }
    }
    if refreshing {
        return Some(
            "Submitting sender-created contexts and checking owned private-output PPOI status.",
        );
    }
    match status {
        PresenceStatus::Healthy => Some("Up to date and following the source."),
        PresenceStatus::Active => {
            Some("PPOI data is ready; one or more outputs still need proof submission or recovery.")
        }
        PresenceStatus::Error => Some("PPOI checks are blocked until the artifact cache rebuilds."),
        PresenceStatus::Unknown => {
            Some("PPOI source or artifact-cache status is not available yet.")
        }
    }
}

const fn ppoi_artifact_phase_label(phase: PoiArtifactCachePhase) -> &'static str {
    match phase {
        PoiArtifactCachePhase::Idle => "Idle",
        PoiArtifactCachePhase::LoadingPersisted => "Loading saved PPOI data",
        PoiArtifactCachePhase::Resetting => "Resetting PPOI data",
        PoiArtifactCachePhase::ResolvingManifest => "Resolving manifest",
        PoiArtifactCachePhase::VerifyingCatalog => "Verifying catalog",
        PoiArtifactCachePhase::Planning => "Planning refresh",
        PoiArtifactCachePhase::DownloadingChunks => "Downloading chunks",
        PoiArtifactCachePhase::ReplayingRanges => "Replaying ranges",
        PoiArtifactCachePhase::Validating => "Validating PPOI data",
        PoiArtifactCachePhase::Persisting => "Saving PPOI data",
        PoiArtifactCachePhase::LiveTailing => "Live tailing",
        PoiArtifactCachePhase::Ready => "Ready",
        PoiArtifactCachePhase::Failed => "Failed",
    }
}

pub(in crate::root) fn ppoi_chunk_progress_label(
    progress: &PoiArtifactCacheProgress,
) -> Option<String> {
    if progress.graph.total_chunks == 0 {
        return None;
    }
    let mut label = format!(
        "{}/{} chunks",
        progress
            .graph
            .verified_chunks
            .min(progress.graph.total_chunks),
        progress.graph.total_chunks
    );
    if let Some(total_bytes) = progress.graph.total_authenticated_encoded_bytes {
        let _ = write!(
            label,
            " · {}/{}",
            format_binary_bytes(progress.graph.verified_encoded_bytes.min(total_bytes)),
            format_binary_bytes(total_bytes)
        );
    } else if progress.graph.verified_encoded_bytes > 0 {
        let _ = write!(
            label,
            " · {} verified",
            format_binary_bytes(progress.graph.verified_encoded_bytes)
        );
    }
    Some(label)
}

pub(in crate::root) fn ppoi_replay_progress_label(
    progress: &PoiArtifactCacheProgress,
) -> Option<String> {
    let start = progress.graph.replay_start_event_index?;
    let end = progress.graph.replay_end_event_index?;
    let replayed = progress
        .graph
        .replayed_event_count
        .min(progress.graph.total_replay_event_count);
    Some(format!(
        "Events {start}-{end} · {replayed}/{} replayed",
        progress.graph.total_replay_event_count
    ))
}

fn ppoi_event_progress_label(progress: &PoiArtifactCacheProgress) -> String {
    match (progress.current_event_index, progress.target_event_index) {
        (Some(current), Some(target)) if current >= target => format!("Event {target}"),
        (Some(current), Some(target)) => format!("Event {} of {}", current.min(target), target),
        (Some(current), None) => format!("Event {current}"),
        (None, Some(target)) => format!("Target event {target}"),
        (None, None) => String::new(),
    }
}

fn ppoi_event_header_label(progress: Option<&PoiArtifactCacheProgress>) -> Option<String> {
    let progress = progress?;
    if progress.total_lists != 1 {
        return None;
    }
    if let [list] = progress.list_progress.as_slice() {
        return ppoi_inline_event_label(list.current_event_index, list.target_event_index);
    }
    ppoi_inline_event_label(progress.current_event_index, progress.target_event_index)
}

fn ppoi_inline_event_label(current: Option<u64>, target: Option<u64>) -> Option<String> {
    match (current, target) {
        (Some(current), Some(target)) if current < target => {
            Some(format!("event {current}/{target}"))
        }
        (Some(current), Some(target)) => Some(format!("event {}", current.min(target))),
        (Some(current), None) => Some(format!("event {current}")),
        (None, Some(target)) => Some(format!("event {target}")),
        (None, None) => None,
    }
}

fn render_ppoi_list_progress_section(progress: &PoiArtifactCacheProgress) -> gpui::Div {
    let ready_lists = progress
        .list_progress
        .iter()
        .filter(|progress| progress.ready_for_wallet_checks)
        .count();

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
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT))
                        .child("POI lists"),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(format!(
                            "{} of {} ready",
                            ready_lists.min(progress.total_lists),
                            progress.total_lists,
                        )),
                ),
        )
        .children(
            progress
                .list_progress
                .iter()
                .map(render_ppoi_list_progress_row),
        )
}

fn render_ppoi_list_progress_row(progress: &PoiArtifactCacheListProgress) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .text_size(px(12.0))
        .child(
            div()
                .min_w_0()
                .font_family(APP_MONO_FONT_FAMILY)
                .text_color(rgb(theme::TEXT_MUTED))
                .truncate()
                .child(short_poi_list_key(progress.list_key.as_slice())),
        )
        .child(
            div()
                .flex_none()
                .font_family(APP_MONO_FONT_FAMILY)
                .text_color(rgb(theme::TEXT_MUTED))
                .child(ppoi_list_event_label(progress).unwrap_or_else(|| "Not ready".to_string())),
        )
}

fn ppoi_list_event_label(progress: &PoiArtifactCacheListProgress) -> Option<String> {
    match (progress.current_event_index, progress.target_event_index) {
        (Some(current), Some(target)) if current < target => {
            Some(format!("Event {current}/{target}"))
        }
        (Some(current), Some(target)) => Some(format!("Event {}", current.min(target))),
        (Some(current), None) => Some(format!("Event {current}")),
        (None, Some(target)) => Some(format!("Event {target}")),
        (None, None) => None,
    }
}

fn short_poi_list_key(bytes: &[u8]) -> String {
    let encoded = hex::encode(bytes);
    if encoded.len() <= 16 {
        return encoded;
    }
    format!("{}...{}", &encoded[..8], &encoded[encoded.len() - 6..])
}

pub(super) const fn ppoi_attention_badge_color(counts: WalletStatusCounts) -> u32 {
    if counts.blocked_shield_outputs > 0
        || counts.ppoi_workflow_status.needs_attention > 0
        || counts.ppoi_workflow_status.recovery_needs_attention > 0
    {
        theme::DANGER
    } else {
        theme::WARNING_BG
    }
}

const fn ppoi_attention_hover_color(counts: WalletStatusCounts) -> u32 {
    if counts.blocked_shield_outputs > 0
        || counts.ppoi_workflow_status.needs_attention > 0
        || counts.ppoi_workflow_status.recovery_needs_attention > 0
    {
        theme::DANGER
    } else {
        theme::WARNING
    }
}

fn ppoi_attention_detail(counts: WalletStatusCounts) -> String {
    let mut items = Vec::with_capacity(2);
    if counts.blocked_shield_outputs > 0 {
        items.push(count_label(
            counts.blocked_shield_outputs,
            "blocked Shield output",
        ));
    }
    let recovery_attention = counts.recoverable_poi_outputs.saturating_add(
        usize::try_from(
            counts
                .ppoi_workflow_status
                .needs_attention
                .saturating_add(counts.ppoi_workflow_status.recovery_needs_attention),
        )
        .unwrap_or(usize::MAX),
    );
    if recovery_attention > 0 {
        items.push(count_label(recovery_attention, "PPOI output needing retry"));
    }
    format!("Review {}", items.join(" and "))
}
