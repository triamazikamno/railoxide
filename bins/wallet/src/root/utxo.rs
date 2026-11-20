use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::hex;
use alloy::primitives::{FixedBytes, U256};
use chrono::{DateTime, Local, Utc};
use gpui::{
    App, AppContext as _, Context, Entity, Focusable, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window,
    div, prelude::FluentBuilder as _, px, relative, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable, StyledExt, WindowExt,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    dialog::DialogFooter,
    input::InputState,
    spinner::Spinner,
    table::{Column, DataTable, TableDelegate, TableState},
    tag::Tag,
    tooltip::Tooltip,
};
use railgun_ui::{format_token_amount, lookup_token, short_address, token_icon_asset_path};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{
    app_button, app_button_base, app_input, app_muted_shimmer_text, app_muted_text,
    app_strong_text, app_text,
};
use ui::dialog::WalletDialogExt as _;
use ui::icons;
use ui::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_LINE_HEIGHT, APP_TEXT_SIZE};
#[cfg(feature = "hardware")]
use wallet_ops::hardware::HardwareDeviceKind;
use wallet_ops::{
    BlockedShieldRescueBlocker, BlockedShieldRescueEligibilityRequest, BlockedShieldRescueInfo,
    BlockedShieldRescueSelfBroadcastRequest, BlockedShieldRescueUtxoId,
    DesktopPrivateSpendAuthorization, ListUtxosOutput, SelfBroadcastGasFeeSelection,
    SelfBroadcastSessionEvent, UtxoOutput, UtxoPpoiState, WalletPpoiWorkflowStatus,
    vault::{ExecutorOperationId, PublicAccountStatus},
};

use super::actions::{UtxoEnd, UtxoHome, UtxoPageDown, UtxoPageUp};
use super::chain_load::ChainUtxoState;
use super::shell::WalletTab;
use super::sidebar::Activity;
use super::spend_authorization::{
    HardwareSpendAuthorizationCompletion, SpendAuthorizationIntent, SpendAuthorizationSummary,
    SpendAuthorizationSummaryRow,
};
use super::stealth_accounts::StealthAccountTarget;
use super::tokens::{native_wrapped_output_labels, parse_address};
use super::{
    SECONDS_PER_HOUR, SECONDS_PER_MINUTE, WalletRoot, centered_message, dialog_max_height,
    rgb_with_alpha, secondary_dialog_content_width, token_label_row,
};

use crate::assets::{RailgunActionIcon, WalletIconSource};

mod shield_refund;
mod utxo_render;
mod utxo_state;

/// Keeps the tracing target of the moved methods at this module's path.
const LOG_TARGET: &str = module_path!();

#[derive(Clone, Copy)]
enum UtxoNavigation {
    PageUp,
    PageDown,
    Home,
    End,
}

const POI_COLUMN_WIDTH: f32 = 190.0;
const BLOCKED_SHIELD_RESCUE_RESOLVING_REASON: &str = "Resolving source transaction origin...";
const BLOCKED_SHIELD_REFUND_IN_FLIGHT_REASON: &str =
    "Blocked Shield refund submission is already in progress.";
const BLOCKED_SHIELD_REFUND_SUBMITTED_REASON: &str =
    "This blocked Shield UTXO is already pending spend.";

#[derive(Clone)]
pub(super) struct BlockedShieldRescueRowState {
    info: BlockedShieldRescueInfo,
    /// What keeps the origin from refunding, when a lookup found that to be the reason.
    blocker: Option<BlockedShieldRescueBlocker>,
    lookup_generation: Option<u64>,
}

impl BlockedShieldRescueRowState {
    pub(super) fn resolving(lookup_generation: u64) -> Self {
        Self {
            info: BlockedShieldRescueInfo {
                eligible: false,
                disabled_reason: Some(BLOCKED_SHIELD_RESCUE_RESOLVING_REASON.to_string()),
                origin_address: None,
                public_account_uuid: None,
                public_account_label: None,
            },
            blocker: None,
            lookup_generation: Some(lookup_generation),
        }
    }

    pub(super) const fn from_info(info: BlockedShieldRescueInfo) -> Self {
        Self {
            info,
            blocker: None,
            lookup_generation: None,
        }
    }

    /// The result of an origin lookup: its info, and the `blocker` it found.
    pub(super) const fn from_lookup(
        info: BlockedShieldRescueInfo,
        blocker: Option<BlockedShieldRescueBlocker>,
    ) -> Self {
        Self {
            info,
            blocker,
            lookup_generation: None,
        }
    }

    pub(super) const fn is_resolving(&self) -> bool {
        self.lookup_generation.is_some()
    }

    pub(super) fn accepts_lookup_result(&self, lookup_generation: u64) -> bool {
        self.lookup_generation == Some(lookup_generation)
    }

    pub(super) const fn info(&self) -> &BlockedShieldRescueInfo {
        &self.info
    }
}

/// What the refund dialog of a blocked Shield shows. It is derived from the row's state on
/// every frame, so the open dialog follows the origin lookup.
pub(super) enum BlockedShieldRefundDialogState {
    /// The origin lookup is running.
    Checking,
    /// The origin can refund.
    Available(BlockedShieldRescueInfo),
    /// The Shield can't be refunded now. A `blocker` says what keeps its origin from refunding.
    Unavailable {
        info: BlockedShieldRescueInfo,
        blocker: Option<BlockedShieldRescueBlocker>,
    },
}

/// Where the action button of the refund dialog leads when the refund is unavailable.
#[derive(Clone, Copy)]
enum BlockedShieldRefundAction {
    OpenStealthAccounts,
    ShowStealthAccount(ExecutorOperationId),
    OpenPublic,
}

/// What the refund dialog adds to the reason a refund is unavailable: its title, how to make
/// the refund possible, and the label and target of the action that leads there.
struct BlockedShieldRefundGuidance {
    title: &'static str,
    fix: Option<SharedString>,
    action: Option<(&'static str, BlockedShieldRefundAction)>,
}

#[derive(Clone)]
pub(super) struct UtxoDisplayRow {
    pub(super) utxo_id: Option<BlockedShieldRescueUtxoId>,
    pub(super) tree_position: String,
    pub(super) token: String,
    pub(super) token_icon_path: Option<WalletIconSource>,
    pub(super) amount: String,
    pub(super) raw_value: Option<U256>,
    pub(super) activity_classification: String,
    pub(super) poi_status: String,
    pub(super) ppoi_state: UtxoPpoiState,
    pub(super) ppoi_last_submission_at: Option<u64>,
    pub(super) poi_spendable: bool,
    pub(super) source_tx_hash: String,
    pub(super) source_block_number: u64,
    pub(super) source_block_timestamp: u64,
    pub(super) spent_tx_hash: Option<String>,
    pub(super) spent_block_number: Option<u64>,
    pub(super) token_address: String,
    pub(super) is_spent: bool,
    pub(super) pending_new: bool,
    pub(super) pending_spent: bool,
    pub(super) local_pending_spent: bool,
    pub(super) blocked_shield_rescue: Option<BlockedShieldRescueInfo>,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(super) struct UtxoFinalityContext {
    head_block: Option<u64>,
    safe_head_block: Option<u64>,
    finality_depth: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BlockFinalityProgress {
    Confirming { elapsed: u64, depth: u64 },
    Safe,
}

impl UtxoFinalityContext {
    pub(super) const fn new(
        head_block: Option<u64>,
        safe_head_block: Option<u64>,
        finality_depth: Option<u64>,
    ) -> Self {
        Self {
            head_block,
            safe_head_block,
            finality_depth,
        }
    }

    pub(super) fn progress(self, block: u64) -> Option<BlockFinalityProgress> {
        let head = self.head_block?;
        let safe_head = self.safe_head_block?;
        let depth = self.finality_depth?;
        if block == 0 || depth == 0 || safe_head > head || head < block {
            return None;
        }
        if safe_head >= block {
            return Some(BlockFinalityProgress::Safe);
        }
        let elapsed = head - block;
        (elapsed < depth).then_some(BlockFinalityProgress::Confirming { elapsed, depth })
    }
}

pub(super) struct UtxoDelegate {
    root: WeakEntity<WalletRoot>,
    rows: Arc<[UtxoDisplayRow]>,
    columns: [Column; 7],
    tx_search_input: Entity<InputState>,
    poi_refreshing: bool,
    poi_retry_session_available: bool,
    finality_context: UtxoFinalityContext,
}

/// The UTXO table, empty until a chain's notes load.
pub(super) fn new_utxo_table(
    root_weak: &WeakEntity<WalletRoot>,
    tx_search_input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<TableState<UtxoDelegate>> {
    cx.new(|cx| {
        TableState::new(
            UtxoDelegate::new(root_weak.clone(), tx_search_input.clone()),
            window,
            cx,
        )
    })
}

impl UtxoDelegate {
    pub(super) fn new(root: WeakEntity<WalletRoot>, tx_search_input: Entity<InputState>) -> Self {
        Self {
            root,
            rows: Arc::from(Vec::<UtxoDisplayRow>::new()),
            columns: [
                Column::new("tree_position", "tree/position")
                    .width(px(120.0))
                    .movable(false),
                Column::new("generated", "generated")
                    .width(px(130.0))
                    .movable(false),
                Column::new("token", "token")
                    .width(px(150.0))
                    .movable(false),
                Column::new("amount", "amount")
                    .width(px(160.0))
                    .movable(false),
                Column::new("poi", "PPOI")
                    .width(px(POI_COLUMN_WIDTH))
                    .movable(false),
                Column::new("source_tx", "source tx")
                    .width(px(201.0))
                    .movable(false),
                Column::new("spent_tx", "spent tx")
                    .width(px(201.0))
                    .movable(false),
            ],
            tx_search_input,
            poi_refreshing: false,
            poi_retry_session_available: false,
            finality_context: UtxoFinalityContext::default(),
        }
    }

    pub(super) fn set_rows(
        &mut self,
        rows: Vec<UtxoDisplayRow>,
        poi_refreshing: bool,
        poi_retry_session_available: bool,
        finality_context: UtxoFinalityContext,
    ) {
        self.rows = Arc::from(rows);
        self.poi_refreshing = poi_refreshing;
        self.poi_retry_session_available = poi_retry_session_available;
        self.finality_context = finality_context;
    }

    pub(super) fn set_finality_context(&mut self, context: UtxoFinalityContext) -> bool {
        if self.finality_context == context {
            return false;
        }
        self.finality_context = context;
        true
    }

    pub(super) const fn set_poi_refreshing(&mut self, poi_refreshing: bool) -> bool {
        if self.poi_refreshing == poi_refreshing {
            return false;
        }
        self.poi_refreshing = poi_refreshing;
        true
    }

    pub(super) fn set_column_widths(&mut self, widths: &[Pixels]) {
        for (column, width) in self.columns.iter_mut().zip(widths.iter().copied()) {
            column.width = width;
        }
    }
}

impl TableDelegate for UtxoDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        utxo_cell(self.columns[col_ix].name.clone())
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        let row = div().id(("row", row_ix));
        if self
            .rows
            .get(row_ix)
            .is_some_and(|row| row.pending_new || row.pending_spent || row.local_pending_spent)
        {
            return row.bg(rgb(theme::WARNING_BG));
        }
        if self.rows.get(row_ix).is_some_and(|row| row.is_spent) {
            return row.bg(rgb(theme::SPENT_ROW_BG));
        }
        row
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        utxo_cell(match col_ix {
            0 => div()
                .text_color(utxo_cell_text_color(row, rgb(theme::TEXT)))
                .child(SharedString::from(row.tree_position.clone()))
                .into_any_element(),
            1 => {
                let finality = pending_finality_display(row, self.finality_context);
                let tooltip = SharedString::from(finality.as_ref().map_or_else(
                    || local_datetime_label(row.source_block_timestamp),
                    |(_, detail)| {
                        format!(
                            "Generated {}. {detail}",
                            local_datetime_label(row.source_block_timestamp)
                        )
                    },
                ));
                div()
                    .id(SharedString::from(format!("wallet-generated-{row_ix}")))
                    .h_full()
                    .flex()
                    .flex_col()
                    .items_start()
                    .justify_center()
                    .gap(px(2.0))
                    .text_color(utxo_cell_text_color(row, rgb(theme::TEXT_MUTED)))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .child(div().text_size(px(13.0)).line_height(px(16.0)).child(
                        SharedString::from(generated_age_label(row.source_block_timestamp)),
                    ))
                    .when_some(finality, |this, (label, _)| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(px(11.0))
                                .line_height(px(14.0))
                                .text_color(rgb(theme::WARNING))
                                .child(Icon::new(RailgunActionIcon::Clock).xsmall())
                                .child(SharedString::from(label)),
                        )
                    })
                    .into_any_element()
            }
            2 => {
                let address = row.token_address.clone();
                let group = SharedString::from(format!("wallet-token-cell-group-{row_ix}"));
                div()
                    .group(group.clone())
                    .id(SharedString::from(format!("wallet-token-cell-{row_ix}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .font_bold()
                    .text_color(utxo_cell_text_color(row, rgb(theme::TEXT)))
                    .child(token_label_row(
                        SharedString::from(row.token.clone()),
                        row.token_icon_path.clone(),
                        px(14.0),
                    ))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "wallet-token-address-copy-action-{row_ix}"
                            )))
                            .group(group.clone())
                            .flex_none()
                            .opacity(0.0)
                            .group_hover(group, |this| this.opacity(1.0))
                            .hover(|this| this.opacity(1.0))
                            .tooltip(|window, cx| {
                                Tooltip::new("Copy token address").build(window, cx)
                            })
                            .child(clipboard_with_toast(
                                SharedString::from(format!(
                                    "wallet-token-address-clipboard-{row_ix}"
                                )),
                                address,
                            )),
                    )
                    .into_any_element()
            }
            3 => div()
                .text_color(utxo_cell_text_color(row, rgb(theme::WARNING)))
                .child(SharedString::from(row.amount.clone()))
                .into_any_element(),
            4 => div()
                .h_full()
                .flex()
                .flex_col()
                .items_start()
                .justify_center()
                .gap(px(2.0))
                .opacity(if row.is_spent { 0.6 } else { 1.0 })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(poi_status_indicator(row, row_ix))
                        .when(should_show_blocked_shield_refund_action(row), |this| {
                            this.child(blocked_shield_refund_action(row, row_ix, self.root.clone()))
                        })
                        .when(should_show_ppoi_retry_action(row), |this| {
                            this.child(ppoi_retry_action(
                                row,
                                row_ix,
                                self.root.clone(),
                                self.poi_refreshing,
                                self.poi_retry_session_available,
                            ))
                        }),
                )
                .when_some(
                    row.ppoi_last_submission_at
                        .filter(|_| should_show_ppoi_submission_age(row)),
                    |this, timestamp| {
                        this.child(
                            div()
                                .text_size(px(11.0))
                                .line_height(px(14.0))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(SharedString::from(format_ppoi_submission_age(
                                    timestamp,
                                    now_epoch_secs(),
                                ))),
                        )
                    },
                )
                .into_any_element(),
            5 => source_tx_cell(
                row,
                row_ix,
                &row.source_tx_hash,
                self.tx_search_input.clone(),
            ),
            _ => match row.spent_tx_hash.as_deref() {
                Some(tx_hash) => tx_hash_cell(
                    row,
                    row_ix,
                    "spent",
                    tx_hash,
                    rgb(theme::DANGER),
                    self.tx_search_input.clone(),
                ),
                None => div()
                    .text_color(rgb(theme::TEXT_SUBTLE))
                    .child("-")
                    .into_any_element(),
            },
        })
    }
}

/// A UTXO table cell's content, centred in the row at the wallet's body text size. The table
/// sizes its own cell text for `Size::Large`, so the cell has to set it again.
fn utxo_cell(content: impl IntoElement) -> gpui::Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .text_size(APP_TEXT_SIZE)
        .line_height(relative(APP_TEXT_LINE_HEIGHT))
        .child(content)
}

pub(super) fn local_pending_spent_summary(count: usize) -> String {
    let noun = if count == 1 { "UTXO" } else { "UTXOs" };
    format!("Locally locked pending submission: {count} {noun}")
}

/// Local pending-submission locks with their two-step clear, shared by the UTXOs tab
/// and the locked notes dialog. `id` prefixes the button IDs.
pub(super) fn local_pending_spent_card(
    id: &str,
    summary: impl Into<SharedString>,
    confirming: bool,
    begin: impl Fn(&mut Window, &mut App) + 'static,
    cancel: impl Fn(&mut Window, &mut App) + 'static,
    confirm: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(if confirming {
            theme::DANGER
        } else {
            theme::BORDER
        }))
        .bg(if confirming {
            rgb_with_alpha(theme::DANGER, 0.08)
        } else {
            rgb(theme::SURFACE)
        })
        .p(px(10.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(app_muted_text(summary).line_height(px(18.0)))
                .child(div().flex_1())
                .when(!confirming, |this| {
                    this.child(
                        app_button(
                            SharedString::from(format!("{id}-clear-local-pending-spent")),
                            "Clear local locks",
                        )
                        .outline()
                        .small()
                        .danger()
                        .on_click(move |_event, window, cx| begin(window, cx)),
                    )
                }),
        )
        .when(confirming, |this| {
            this.child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(17.0))
                    .text_color(rgb(theme::DANGER))
                    .child("This only clears local submitted-transaction locks. If the original transaction later confirms, these UTXOs may fail simulation or become spent again."),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_button(
                            SharedString::from(format!("{id}-cancel-clear-local-pending-spent")),
                            "Cancel",
                        )
                        .outline()
                        .small()
                        .on_click(move |_event, window, cx| cancel(window, cx)),
                    )
                    .child(
                        app_button(
                            SharedString::from(format!("{id}-confirm-clear-local-pending-spent")),
                            "Clear local locks",
                        )
                        .small()
                        .danger()
                        .on_click(move |_event, window, cx| confirm(window, cx)),
                    ),
            )
        })
}

fn poi_status_indicator(row: &UtxoDisplayRow, row_ix: usize) -> gpui::AnyElement {
    if is_shield_blocked_poi_status(&row.poi_status) {
        return div()
            .id(SharedString::from(format!(
                "wallet-poi-shield-blocked-{row_ix}"
            )))
            .flex_none()
            .tooltip(|window, cx| Tooltip::new("ShieldBlocked").build(window, cx))
            .child(
                Icon::empty()
                    .path(icons::ban_icon_path())
                    .small()
                    .text_color(rgb(theme::DANGER)),
            )
            .into_any_element();
    }
    let tag = if row.poi_spendable {
        Tag::success()
    } else {
        Tag::warning()
    };
    let (label, detail, show_clock) = match shield_poi_wait_display(row, now_epoch_secs()) {
        Some(display) => (display.label, display.detail, true),
        None => (
            row.poi_status.clone(),
            ppoi_row_state_detail_with_submission(
                row.ppoi_state,
                row.is_spent,
                row.ppoi_last_submission_at,
            ),
            false,
        ),
    };
    div()
        .id(SharedString::from(format!("wallet-poi-status-{row_ix}")))
        .tooltip(move |window, cx| Tooltip::new(detail).build(window, cx))
        .child(
            tag.small().outline().child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .when(show_clock, |this| {
                        this.child(Icon::new(RailgunActionIcon::Clock).xsmall())
                    })
                    .child(SharedString::from(label)),
            ),
        )
        .into_any_element()
}

pub(super) struct ShieldPoiWaitDisplay {
    pub(super) label: String,
    pub(super) detail: &'static str,
    pub(super) delayed: bool,
}

pub(super) fn shield_poi_wait_display(
    row: &UtxoDisplayRow,
    now_epoch_secs: u64,
) -> Option<ShieldPoiWaitDisplay> {
    if row.activity_classification != "Shield"
        || row.poi_spendable
        || row.is_spent
        || row.pending_new
        || row.pending_spent
        || row.local_pending_spent
        || row.source_block_timestamp == 0
        || !matches!(
            row.ppoi_state,
            UtxoPpoiState::Missing | UtxoPpoiState::Unknown | UtxoPpoiState::ProofSubmitted
        )
    {
        return None;
    }

    shield_poi_wait_time_display(row.source_block_timestamp, now_epoch_secs)
}

pub(super) fn shield_poi_wait_time_display(
    source_block_timestamp: u64,
    now_epoch_secs: u64,
) -> Option<ShieldPoiWaitDisplay> {
    let deadline = source_block_timestamp.checked_add(SECONDS_PER_HOUR)?;
    let remaining_secs = deadline
        .saturating_sub(now_epoch_secs)
        .min(SECONDS_PER_HOUR);
    if remaining_secs == 0 {
        return Some(ShieldPoiWaitDisplay {
            label: "Taking longer than usual".to_string(),
            detail: "Shield PPOI verification is taking longer than usual. Timing may vary.",
            delayed: true,
        });
    }

    let remaining_minutes = remaining_secs.div_ceil(SECONDS_PER_MINUTE);
    let label = if remaining_minutes >= 60 {
        "~1h".to_string()
    } else {
        format!("~{remaining_minutes}m")
    };
    Some(ShieldPoiWaitDisplay {
        label,
        detail: "Estimated time remaining for Shield PPOI verification. This typically takes about one hour; timing may vary.",
        delayed: false,
    })
}

pub(super) fn pending_finality_display(
    row: &UtxoDisplayRow,
    context: UtxoFinalityContext,
) -> Option<(String, String)> {
    let (block_number, progress_subject, pending_object, indexing_detail) = if row.pending_spent {
        (
            row.spent_block_number?,
            "spend",
            "spend",
            "The spend transaction reached the chain safe head. Waiting for the wallet snapshot to mark this output spent.",
        )
    } else if row.pending_new {
        (
            row.source_block_number,
            "receive",
            "output",
            "The source transaction reached the chain safe head. Waiting for the wallet snapshot to include this output.",
        )
    } else {
        return None;
    };
    let (elapsed, finality_depth) = match context.progress(block_number)? {
        BlockFinalityProgress::Safe => {
            return Some(("Indexing".to_string(), indexing_detail.to_string()));
        }
        BlockFinalityProgress::Confirming { elapsed, depth } => (elapsed, depth),
    };
    Some((
        format!("{elapsed}/{finality_depth} blocks"),
        format!(
            "Pending {progress_subject}: {elapsed} of {finality_depth} finality blocks elapsed. This {pending_object} remains pending until it reaches the chain safe head."
        ),
    ))
}

fn ppoi_retry_action(
    row: &UtxoDisplayRow,
    row_ix: usize,
    root: WeakEntity<WalletRoot>,
    refreshing: bool,
    session_available: bool,
) -> gpui::AnyElement {
    let tooltip = ppoi_retry_tooltip(row.ppoi_state);
    div()
        .child(
            app_button_base(SharedString::from(format!("wallet-retry-poi-{row_ix}")))
                .xsmall()
                .disabled(!session_available)
                .tooltip(tooltip)
                .child(ppoi_row_retry_label(refreshing))
                .on_click(move |_event, _window, cx| {
                    cx.stop_propagation();
                    let _ = root.update(cx, |root, cx| {
                        WalletRoot::retry_poi_submissions(root.selected_chain_session(), cx);
                    });
                }),
        )
        .into_any_element()
}

fn source_tx_cell(
    row: &UtxoDisplayRow,
    row_ix: usize,
    tx_hash: &str,
    tx_search_input: Entity<InputState>,
) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(activity_classification_icon(row, row_ix))
        .child(tx_hash_cell(
            row,
            row_ix,
            "source",
            tx_hash,
            rgb(theme::TEAL),
            tx_search_input,
        ))
        .into_any_element()
}

fn activity_classification_icon(row: &UtxoDisplayRow, row_ix: usize) -> gpui::AnyElement {
    let (path, color, label) = activity_classification_icon_style(&row.activity_classification);
    div()
        .id(SharedString::from(format!(
            "wallet-source-tx-classification-{row_ix}"
        )))
        .flex_none()
        .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
        .child(Icon::empty().path(path).small().text_color(rgb(color)))
        .into_any_element()
}

pub(super) fn activity_classification_icon_style(
    classification: &str,
) -> (&'static str, u32, &'static str) {
    match classification {
        "Shield" => (icons::shield_plus_icon_path(), theme::SUCCESS, "Shield"),
        "BlockedShield" | "Blocked Shield" => (
            icons::shield_alert_icon_path(),
            theme::DANGER,
            "Blocked Shield",
        ),
        _ => (
            icons::shield_check_icon_path(),
            theme::TEXT,
            "Private Output",
        ),
    }
}

fn tx_hash_cell(
    row: &UtxoDisplayRow,
    row_ix: usize,
    kind: &'static str,
    tx_hash: &str,
    color: gpui::Rgba,
    tx_search_input: Entity<InputState>,
) -> gpui::AnyElement {
    let display_hash = short_hash(tx_hash);
    let search_hash = tx_hash.to_string();
    let group = SharedString::from(format!("wallet-{kind}-tx-group-{row_ix}"));

    div()
        .group(group.clone())
        .id(SharedString::from(format!("wallet-{kind}-tx-{row_ix}")))
        .flex()
        .items_center()
        .gap_1()
        .child(
            div()
                .id(SharedString::from(format!(
                    "wallet-{kind}-tx-copy-{row_ix}"
                )))
                .flex_none()
                .font_family(APP_MONO_FONT_FAMILY)
                .text_size(px(12.0))
                .text_color(utxo_cell_text_color(row, color))
                .child(SharedString::from(display_hash)),
        )
        .child(
            div()
                .id(SharedString::from(format!(
                    "wallet-{kind}-tx-actions-{row_ix}"
                )))
                .group(group.clone())
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .opacity(0.0)
                .group_hover(group, |this| this.opacity(1.0))
                .hover(|this| this.opacity(1.0))
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "wallet-{kind}-tx-copy-action-{row_ix}"
                        )))
                        .tooltip(|window, cx| {
                            Tooltip::new("Copy transaction hash").build(window, cx)
                        })
                        .child(clipboard_with_toast(
                            SharedString::from(format!("wallet-{kind}-tx-clipboard-{row_ix}")),
                            tx_hash.to_string(),
                        )),
                )
                .child(
                    app_button_base(SharedString::from(format!(
                        "wallet-{kind}-tx-search-{row_ix}"
                    )))
                    .ghost()
                    .xsmall()
                    .accessibility_label("Filter by this transaction")
                    .tooltip("Filter by this transaction")
                    .icon(IconName::Search)
                    .on_click(move |_event, window, cx| {
                        tx_search_input.update(cx, |input, cx| {
                            input.replace_all(search_hash.clone(), window, cx);
                        });
                    }),
                ),
        )
        .into_any_element()
}

fn blocked_shield_refund_action(
    row: &UtxoDisplayRow,
    row_ix: usize,
    root: WeakEntity<WalletRoot>,
) -> gpui::AnyElement {
    let Some(rescue) = row.blocked_shield_rescue.as_ref() else {
        return div().into_any_element();
    };
    let mut button = app_button_base(SharedString::from(format!(
        "wallet-blocked-shield-refund-{row_ix}"
    )))
    .xsmall()
    .danger()
    .loading(blocked_shield_refund_origin_resolving(row))
    .child("Refund");
    if rescue.eligible || can_start_blocked_shield_origin_resolution(row, rescue) {
        let row = row.clone();
        button = button.on_click(move |_event, window, cx| {
            cx.stop_propagation();
            let row = row.clone();
            let _ = root.update(cx, |root, cx| {
                root.begin_blocked_shield_refund(&row, window, cx);
            });
        });
        if !rescue.eligible {
            button = button.tooltip(
                rescue
                    .disabled_reason
                    .clone()
                    .unwrap_or_else(|| "Check source transaction origin before refund".to_owned()),
            );
        }
    } else {
        let reason = rescue
            .disabled_reason
            .clone()
            .unwrap_or_else(|| "Blocked Shield refund is unavailable.".to_string());
        button = button.disabled(true).tooltip(reason);
    }
    div().child(button).into_any_element()
}

fn blocked_shield_refund_authorization_summary(
    row: &UtxoDisplayRow,
    rescue: &BlockedShieldRescueInfo,
    origin_address: &str,
    device_only: bool,
) -> SpendAuthorizationSummary {
    let gas_payer = rescue
        .public_account_label
        .as_ref()
        .map_or_else(|| origin_address.to_string(), std::clone::Clone::clone);
    SpendAuthorizationSummary::new(
        "Blocked Shield refund",
        if device_only {
            "Approve this refund on your hardware wallet."
        } else {
            "Enter your vault password to authorize this refund."
        },
        vec![
            SpendAuthorizationSummaryRow::new("Amount", format!("{} {}", row.amount, row.token))
                .with_icon(row.token_icon_path.clone()),
            SpendAuthorizationSummaryRow::new("Recipient", origin_address.to_string())
                .with_shortened_copyable(),
            SpendAuthorizationSummaryRow::new("Delivery", "Self-broadcast"),
            SpendAuthorizationSummaryRow::new("Source transaction", row.source_tx_hash.clone()),
            SpendAuthorizationSummaryRow::new("Origin gas payer", gas_payer),
        ],
    )
}

pub(super) fn should_show_blocked_shield_refund_action(row: &UtxoDisplayRow) -> bool {
    is_shield_blocked_poi_status(&row.poi_status) && row.blocked_shield_rescue.is_some()
}

pub(super) fn blocked_shield_refund_action_available(row: &UtxoDisplayRow) -> bool {
    let Some(rescue) = row.blocked_shield_rescue.as_ref() else {
        return false;
    };
    rescue.eligible || can_start_blocked_shield_origin_resolution(row, rescue)
}

pub(super) fn blocked_shield_refund_origin_resolving(row: &UtxoDisplayRow) -> bool {
    row.blocked_shield_rescue.as_ref().is_some_and(|rescue| {
        rescue.disabled_reason.as_deref() == Some(BLOCKED_SHIELD_RESCUE_RESOLVING_REASON)
    })
}

fn is_shield_blocked_poi_status(status: &str) -> bool {
    status == "ShieldBlocked"
}

fn utxo_cell_text_color(row: &UtxoDisplayRow, color: gpui::Rgba) -> gpui::Rgba {
    if row.is_spent {
        rgb(theme::SPENT_TEXT)
    } else if row.pending_new || row.pending_spent || row.local_pending_spent {
        rgb(theme::WARNING)
    } else {
        color
    }
}

pub(super) fn should_focus_utxo_table(
    active_activity: Activity,
    active_wallet_tab: WalletTab,
    state: Option<&ChainUtxoState>,
) -> bool {
    active_activity == Activity::Wallet
        && active_wallet_tab.shows_utxos()
        && state.is_some_and(ChainUtxoState::renders_table)
}

pub(super) fn should_refresh_utxo_ages(
    active_activity: Activity,
    active_wallet_tab: WalletTab,
    has_snapshot: bool,
) -> bool {
    active_activity == Activity::Wallet && active_wallet_tab.shows_utxos() && has_snapshot
}

pub(super) fn recoverable_poi_candidate_count(snapshot: &ListUtxosOutput) -> usize {
    snapshot
        .utxos
        .iter()
        .filter(|row| is_recoverable_poi_candidate(row))
        .count()
}

fn is_recoverable_poi_candidate(row: &UtxoOutput) -> bool {
    row.is_ppoi_retry_eligible()
}

pub(super) fn should_show_ppoi_retry_action(row: &UtxoDisplayRow) -> bool {
    !row.is_spent
        && !row.pending_new
        && !row.pending_spent
        && !row.local_pending_spent
        && row.activity_classification == "Private Output"
        && row.ppoi_state.retry_eligible()
}

pub(super) fn should_show_ppoi_submission_age(row: &UtxoDisplayRow) -> bool {
    row.ppoi_last_submission_at
        .is_some_and(|timestamp| timestamp != 0)
        && should_show_ppoi_retry_action(row)
}

pub(super) const fn global_poi_retry_available(
    session_available: bool,
    _refreshing: bool,
    workflow_needs_attention: u64,
    owned_retry_candidates: usize,
) -> bool {
    session_available && (owned_retry_candidates > 0 || workflow_needs_attention > 0)
}

pub(super) const fn poi_retry_button_label(refreshing: bool) -> &'static str {
    if refreshing {
        "Queue PPOI retry"
    } else {
        "Retry PPOI submissions"
    }
}

pub(super) const fn ppoi_row_retry_label(refreshing: bool) -> &'static str {
    if refreshing { "Queue retry" } else { "Retry" }
}

pub(super) const fn ppoi_state_detail(state: UtxoPpoiState) -> &'static str {
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

pub(super) fn ppoi_row_state_detail_with_submission(
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

pub(super) const fn ppoi_workflow_status_title(
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

pub(super) fn ppoi_workflow_status_detail(status: WalletPpoiWorkflowStatus) -> String {
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

pub(super) const fn ppoi_retry_tooltip(state: UtxoPpoiState) -> &'static str {
    ppoi_state_detail(state)
}

pub(super) fn display_rows_from_output(
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

pub(super) fn blocked_shield_rescue_display_rows(
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

pub(super) fn apply_blocked_shield_rescue_rows(
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

fn blocked_shield_rescue_utxo_id_from_output(
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

fn active_blocked_shield_rescue_utxo_id_from_output(
    row: &UtxoOutput,
) -> Option<BlockedShieldRescueUtxoId> {
    if row.is_spent || row.pending_new || row.pending_spent || row.local_pending_spent {
        return None;
    }
    blocked_shield_rescue_utxo_id_from_output(row)
}

fn can_start_blocked_shield_origin_resolution(
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

fn blocked_shield_rescue_info_from_eligibility(
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

/// The title, fix and action the refund dialog shows for the `blocker` of an unavailable
/// refund. `stealth_accounts` says whether the selected chain has Stealth accounts to open, and
/// `native` is the symbol of its native currency.
fn blocked_shield_refund_guidance(
    blocker: Option<&BlockedShieldRescueBlocker>,
    stealth_accounts: bool,
    native: Option<&str>,
) -> BlockedShieldRefundGuidance {
    match blocker {
        None => BlockedShieldRefundGuidance {
            title: "Refund unavailable",
            fix: None,
            action: None,
        },
        Some(BlockedShieldRescueBlocker::OriginUnknown) if stealth_accounts => {
            BlockedShieldRefundGuidance {
                title: "Origin account missing",
                fix: Some(
                    "A blocked Shield is refunded to the account that made it, and that account \
                     pays the gas. If it was one of your stealth accounts, restore it in Stealth \
                     accounts. Otherwise add it to Public."
                        .into(),
                ),
                action: Some((
                    "Open Stealth accounts",
                    BlockedShieldRefundAction::OpenStealthAccounts,
                )),
            }
        }
        Some(BlockedShieldRescueBlocker::OriginUnknown) => BlockedShieldRefundGuidance {
            title: "Origin account missing",
            fix: Some(
                "A blocked Shield is refunded to the account that made it, and that account pays \
                 the gas. Add it to Public, then refund."
                    .into(),
            ),
            action: None,
        },
        Some(BlockedShieldRescueBlocker::OriginStealth { operation, .. }) => {
            BlockedShieldRefundGuidance {
                title: "Stealth account not in Public",
                fix: Some(
                    format!(
                        "Add it to Public, send it {} for gas, then refund.",
                        native.unwrap_or("native currency")
                    )
                    .into(),
                ),
                action: Some((
                    "Show stealth account",
                    BlockedShieldRefundAction::ShowStealthAccount(*operation),
                )),
            }
        }
        Some(BlockedShieldRescueBlocker::OriginInactive { .. }) => BlockedShieldRefundGuidance {
            title: "Origin account inactive",
            fix: Some("Activate it, then refund.".into()),
            action: Some(("Open Public", BlockedShieldRefundAction::OpenPublic)),
        },
    }
}

/// The Close button of the refund dialog.
fn blocked_shield_refund_close_button() -> Button {
    app_button("wallet-blocked-shield-refund-unavailable-close", "Close")
        .on_click(|_, window, cx| window.close_dialog(cx))
}

/// The "Origin account" row of the refund dialog: the address beside its copy button.
fn blocked_shield_refund_origin_row(address: String) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text("Origin account"))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    app_text(address.clone())
                        .font_family(APP_MONO_FONT_FAMILY)
                        .flex_1()
                        .min_w_0()
                        .truncate(),
                )
                .child(
                    clipboard_with_toast("wallet-blocked-shield-refund-origin-copy", address)
                        .tooltip("Copy origin account address"),
                ),
        )
}

/// The footer of the refund dialog for an available refund: Close beside the button that
/// goes on to the refund's authorization.
fn blocked_shield_refund_available_footer(
    root: &Entity<WalletRoot>,
    utxo_id: BlockedShieldRescueUtxoId,
) -> gpui::AnyElement {
    let root = root.clone();
    DialogFooter::new()
        .child(blocked_shield_refund_close_button().ghost())
        .child(
            app_button("wallet-blocked-shield-refund-continue", "Refund…")
                .primary()
                .debug_selector(|| "wallet-blocked-shield-refund-continue".into())
                .on_click(move |_, window, cx| {
                    root.update(cx, |root, cx| {
                        window.close_dialog(cx);
                        // The row is read again, so the refund goes on only while the row
                        // still offers it.
                        if let Some(row) = root.active_blocked_shield_rescue_display_row(utxo_id) {
                            root.begin_blocked_shield_refund(&row, window, cx);
                        }
                    });
                }),
        )
        .into_any_element()
}

/// The footer of the refund dialog for an unavailable refund: Close alone, or Close beside
/// the `action` that leads to the fix. The action is disabled while it can't be followed.
fn blocked_shield_refund_unavailable_footer(
    root: &Entity<WalletRoot>,
    action: Option<(&'static str, BlockedShieldRefundAction)>,
    cx: &App,
) -> gpui::AnyElement {
    let close = blocked_shield_refund_close_button();
    let Some((label, action)) = action else {
        return close.into_any_element();
    };
    let available = root
        .read(cx)
        .can_follow_blocked_shield_refund_action(action, cx);
    let root = root.clone();
    DialogFooter::new()
        .child(close.ghost())
        .child(
            app_button("wallet-blocked-shield-refund-unavailable-action", label)
                .primary()
                .disabled(!available)
                .debug_selector(|| "wallet-blocked-shield-refund-unavailable-action".into())
                .on_click(move |_, window, cx| {
                    root.update(cx, |root, cx| {
                        // The dialog stays open while the action can't be followed, so its
                        // guidance isn't lost.
                        if !root.can_follow_blocked_shield_refund_action(action, cx) {
                            return;
                        }
                        // Close every dialog, so nothing opens underneath one. This dialog
                        // opened inside a Refund click, over nothing or over "Private asset
                        // status". A dialog opened since covers this button until it is
                        // dismissed, so no other dialog is open now.
                        window.close_all_dialogs(cx);
                        root.follow_blocked_shield_refund_action(action, window, cx);
                    });
                }),
        )
        .into_any_element()
}

const fn blocked_shield_rescue_error_info(error: String) -> BlockedShieldRescueInfo {
    BlockedShieldRescueInfo {
        eligible: false,
        disabled_reason: Some(error),
        origin_address: None,
        public_account_uuid: None,
        public_account_label: None,
    }
}

fn blocked_shield_rescue_in_flight_info(
    base: Option<&BlockedShieldRescueInfo>,
) -> BlockedShieldRescueInfo {
    BlockedShieldRescueInfo {
        eligible: false,
        disabled_reason: Some(BLOCKED_SHIELD_REFUND_IN_FLIGHT_REASON.to_string()),
        origin_address: base.and_then(|info| info.origin_address.clone()),
        public_account_uuid: base.and_then(|info| info.public_account_uuid.clone()),
        public_account_label: base.and_then(|info| info.public_account_label.clone()),
    }
}

fn blocked_shield_rescue_submitted_info() -> BlockedShieldRescueInfo {
    BlockedShieldRescueInfo {
        eligible: false,
        disabled_reason: Some(BLOCKED_SHIELD_REFUND_SUBMITTED_REASON.to_string()),
        origin_address: None,
        public_account_uuid: None,
        public_account_label: None,
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

fn display_row_from_utxo(chain_id: u64, row: &UtxoOutput) -> UtxoDisplayRow {
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

fn generated_age_label(timestamp: u64) -> String {
    let age_secs = now_epoch_secs().saturating_sub(timestamp);
    ui::format::format_relative_age(Duration::from_secs(age_secs))
}

pub(super) fn format_ppoi_submission_age(timestamp: u64, now_epoch_secs: u64) -> String {
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

fn local_datetime_label(timestamp: u64) -> String {
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

pub(super) fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn short_hash(hash: &str) -> String {
    if hash.len() <= 14 {
        return hash.to_string();
    }
    format!("{}...{}", &hash[..8], &hash[hash.len() - 6..])
}
