//! The Private tab's swap card, each swap's order detail, and the actions its state allows:
//! cancel an open order, retry after an attempt ended, recover stranded funds through Stealth
//! accounts, and remove a swap that ended from the Private tab.

use std::sync::Arc;

use alloy::primitives::{Address, U256};
use gpui::{
    App, ClickEvent, Context, Entity, FontWeight, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, img, prelude::FluentBuilder as _, px, rems, rgb,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
    alert::Alert,
    button::{ButtonVariant, ButtonVariants},
    collapsible::Collapsible,
    dialog::DialogButtonProps,
    tooltip::Tooltip,
};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{
    app_button, app_button_base, app_muted_shimmer_text, app_muted_text, app_strong_text, app_text,
};
use ui::dialog::WalletDialogExt as _;
use ui::theme;
use wallet_ops::{
    DesktopPrivateSpendAuthorization, ExecutorAsset, ExecutorPaidRecoveryRequest,
    ExecutorRecoveryFeeEstimate, PublicBroadcasterCandidate, PublicBroadcasterSelection,
    PublicBroadcasterWaku, SwapAccountReplacement, SwapOrderState,
    cow::OrderUid,
    vault::{
        BridgeDelivery, BridgeOrderTerms, BridgeProvider, ExecutorOperationId,
        ExecutorPayloadState, ExecutorRecord, NonEvmDelivery, SwapAccountRole, SwapAccountUse,
        SwapAdmissionEvidence, SwapBridgeOutcome, SwapDelivery, SwapOrderRecord,
        SwapPreHookDeathCause, SwapSubmissionStatus, SwapUseId, SwapUseRecord, SwapUseRelease,
        SwapUseRole, swap_account_refusal,
    },
};

use super::dialog::{SwapDialogView, provider_mention, settled_by_cow};
use super::form::{
    ReplacedSwapAccount, SetupSide, broadcaster_result_problem, format_gwei, gas_share_name,
    network_name, swap_steps as review_steps, swap_steps_hint,
};
use super::model::{
    SwapActions, SwapAttempts, SwapIdentity, SwapLabels, SwapOrderGroup, SwapSetupProgress,
    SwapStage, SwapStep, SwapStepAccount, bridge_sent_amount, card_line, needs_executed_fee,
    prepared_swap_use, private_delivery_credit, provider_name, record_outcome_order, record_swaps,
    reported_fill_note, setup_unresolved_below, speed_up_shown, swap_actions, swap_order_group,
    swap_order_stage, swap_order_status, swap_outcome, swap_private_delivery, swap_private_minimum,
    swap_setup_progress, swap_steps, swap_use_destination, swap_valid_to,
};
use super::setup_progress::{SetupProgress, SetupProgressEnd};
use super::{
    PrivateSwapsView, SWAP_BROADCASTER_REPUBLISH_INTERVAL, SWAP_BROADCASTER_RESPONSE_TIMEOUT,
    SetupRetryAvailability, SwapAction, SwapJobKind, local_date_time_label, local_time_label,
    non_evm_amount, sets_up_destination, short_receiver, swap_delivery, swap_recovery_token,
    swap_tokens,
};
use crate::assets::WalletIconSource;
use crate::root::broadcaster_picker::broadcaster_candidate_label;
use crate::root::dialog_max_height;
use crate::root::private_broadcaster::{
    broadcaster_preference_control, private_broadcaster_stage_detail, private_broadcaster_stage_id,
    private_broadcaster_stage_label, render_broadcaster_preference_control,
};
use crate::root::public_action::PublicActionStepStatus;
use crate::root::public_broadcaster_cost::private_broadcaster_context_row_with_action;
use crate::root::spend_authorization::{
    SpendAuthorizationSteps, SpendAuthorizationSummary, SpendAuthorizationSummaryRow,
    render_spend_authorization_steps,
};
use crate::root::stealth_accounts::StealthAccountTarget;
use crate::root::submission_progress::{
    SubmissionProgressGroup, SubmissionProgressStep, SubmissionProgressSubstep,
    render_submission_progress_groups, render_submission_progress_stepper,
};
use crate::root::ui_helpers::{network_mention, now_unix};
use crate::root::utxo::short_hash;

mod attempt_outcome;
mod cancellation;
mod detail_render;
mod setup_facts;
mod status_checks;

/// Why Cancel order… waits while a setup below the open order is unconfirmed.
const CANCEL_WAITS_FOR_SETUP: &str = "The order can be cancelled once the stealth account's setup is confirmed. An order left alone expires at no cost.";

/// Why Swap again waits after an order placed on a setup that isn't confirmed yet.
const RETRY_WAITS_FOR_SETUP: &str =
    "You can swap again once the stealth account's setup is confirmed.";

/// A broadcaster's fee for an early cancellation, estimated before the confirmation.
pub(super) struct CancelQuote {
    operation: ExecutorOperationId,
    estimate: ExecutorRecoveryFeeEstimate,
}

/// The cancellation the user approved.
#[derive(Clone)]
pub(super) struct CancelApproval {
    pub(super) operation: ExecutorOperationId,
    candidate: PublicBroadcasterCandidate,
    maximum_private_fee: U256,
    waku: PublicBroadcasterWaku,
}

/// What cancelling a swap's preparation left of its claims, as its detail reports it.
pub(super) struct CancelledPreparation {
    pub(super) operation: ExecutorOperationId,
    /// The swap's title, as it was while its preparation was shown.
    pub(super) title: String,
    /// The swap's own account, then a private Bridge swap's destination account.
    pub(super) accounts: Vec<CancelledAccount>,
}

impl CancelledPreparation {
    pub(super) fn needs_attention(&self) -> bool {
        self.accounts
            .iter()
            .any(|account| account.unknown || account.release == SwapUseRelease::IssuedWorkRemains)
    }
}

/// Which account of a cancelled preparation the detail describes.
#[derive(Clone, Copy)]
enum CancelledAccountRole {
    Source,
    Destination,
}

/// One stealth account of a cancelled preparation.
pub(super) struct CancelledAccount {
    role: CancelledAccountRole,
    network: String,
    /// `None` for a new account whose address wasn't derived.
    pub(super) account: Option<SwapStepAccount>,
    /// The swap reserved the account fresh. Otherwise it reused an existing one.
    pub(super) fresh: bool,
    pub(super) release: SwapUseRelease,
    /// Destination records are unavailable until their owning session is loaded.
    unknown: bool,
}

impl CancelledAccount {
    /// What the cancellation left of the account. An existing account is released or kept
    /// reserved, and never retired. A fresh one stays in Stealth accounts, with its guards
    /// while a setup it sent can still confirm. A fresh destination isn't used again.
    /// `source_available` tells whether the Source picker offers the source account now,
    /// which a clean cancellation alone doesn't promise.
    pub(super) const fn outcome(&self, source_available: bool) -> &'static str {
        if self.unknown {
            return "Load this network to check its signed work. Cancellation preserves any outstanding nonce and fee-note guards.";
        }
        let source = matches!(self.role, CancelledAccountRole::Source);
        match (self.fresh, self.release) {
            (false, SwapUseRelease::Released) if !source || source_available => {
                "Released. You can choose this account for another swap."
            }
            (false, SwapUseRelease::Released) => {
                "Released from this swap. It can't be chosen for another swap right now."
            }
            (false, SwapUseRelease::IssuedWorkRemains) => {
                "Still reserved. A shield it signed for this swap can still run, so its nonce guard stays until that is resolved."
            }
            (true, SwapUseRelease::Released) if source && source_available => {
                "Preparation stopped with no unresolved signed work. You can choose this account for another swap."
            }
            (true, SwapUseRelease::Released) if source => {
                "Preparation stopped with no unresolved signed work. This new account stays in Stealth accounts. It can be chosen for another swap once its setup is confirmed and no other swap holds it."
            }
            (true, SwapUseRelease::Released) => {
                "Preparation stopped with no unresolved signed work. This new account stays in Stealth accounts and isn't used for another swap."
            }
            (true, SwapUseRelease::IssuedWorkRemains) => {
                "Unresolved. Its setup was sent and may still confirm and charge its fee. Its nonce and fee-note guards stay until that is resolved, and the account stays in Stealth accounts for recovery."
            }
        }
    }
}

/// What a setup's confirmation step says until the setup is seen included.
const SETUP_AWAITS_INCLUSION: &str = "Waiting for the setup to be included.";

/// One stealth account's setup, as the detail shows it while the swap is at its setup.
struct SetupColumn {
    side: SetupSide,
    /// The account's network and the account, which head each setup of a private Bridge swap.
    /// A swap with one account has no heading.
    heading: Option<(String, Option<SwapStepAccount>)>,
    progress: SwapSetupProgress,
    /// The account's setup among the swap's steps, whose status and text tell its outcome.
    step: SwapStep,
    /// What the confirmation waits for, once the setup is seen included.
    confirmation: Option<String>,
}

impl Render for PrivateSwapsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl gpui::IntoElement {
        self.render_card(&cx.entity(), cx).map_or_else(
            || gpui::Empty.into_any_element(),
            gpui::IntoElement::into_any_element,
        )
    }
}

/// The attempt of the record's latest swap that holds its bridge evidence: the one that
/// delivered its trade, or its latest order while nothing is delivered.
fn latest_bridge_evidence(record: &ExecutorRecord) -> Option<&SwapOrderRecord> {
    let latest = record.swap()?.orders().last()?;
    Some(SwapAttempts::ending_at(record, latest).bridge_evidence())
}

/// Whether the record's latest swap bridges with Across, whose refund an explicit check
/// verifies on this network.
fn refunds_across_deposit(record: &ExecutorRecord) -> bool {
    latest_bridge_evidence(record)
        .is_some_and(|order| matches!(order.bridge(), Some(BridgeOrderTerms::Across(_))))
}

/// Whether the record's latest swap bridges with Across and only Across reported its
/// delivery, which an explicit check asks Across about again.
fn reported_by_across(record: &ExecutorRecord) -> bool {
    refunds_across_deposit(record)
        && latest_bridge_evidence(record).is_some_and(|order| {
            matches!(
                order.observations().bridge_outcome,
                Some(SwapBridgeOutcome::DeliveredReported { .. })
            )
        })
}

/// One attempt of a swap, as the detail's Orders section lists it.
pub(super) struct SwapAttemptRow {
    /// "Order 1 · Bridge deposit", or the attempt's own state after its number.
    pub(super) label: String,
    pub(super) order: OrderUid,
    /// "Minimum 248.71 USDC · valid until Today 14:32".
    pub(super) detail: String,
}

/// A hash, shortened, with a control that copies it in full.
pub(super) fn hash_row(
    label: impl Into<SharedString>,
    hash: String,
    copy_id: SharedString,
    tooltip: &'static str,
) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .child(app_muted_text(label).flex_none())
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(app_strong_text(short_hash(&hash)).font_family(theme::APP_MONO_FONT_FAMILY))
                .child(clipboard_with_toast(copy_id, hash).tooltip(tooltip)),
        )
}

/// A swap's receiver row, laid out like [`hash_row`]: the wallet's `label` for it when it has
/// one, and its `short` form with a control that copies `full`.
fn receiver_fact_row(full: String, short: String, label: Option<String>) -> gpui::Div {
    let copy_id = SharedString::from(format!("swap-receiver-{full}-copy"));
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .debug_selector(|| "swap-detail-receiver".into())
        .child(app_muted_text("Receiver").flex_none())
        .child(
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap_1()
                .children(label.map(|label| app_text(label).min_w_0().truncate()))
                .child(
                    app_strong_text(short)
                        .flex_none()
                        .font_family(theme::APP_MONO_FONT_FAMILY),
                )
                .child(clipboard_with_toast(copy_id, full).tooltip("Copy receiver address")),
        )
}

/// An address, shortened, with a control that copies it in full, laid out like [`hash_row`].
pub(super) fn address_row(
    label: &'static str,
    address: Address,
    tooltip: &'static str,
) -> gpui::Div {
    let short = short_receiver(address);
    let address = address.to_checksum(None);
    let copy_id = SharedString::from(format!("swap-address-{address}-copy"));
    fact_row(
        label,
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(app_strong_text(short).font_family(theme::APP_MONO_FONT_FAMILY))
            .child(clipboard_with_toast(copy_id, address).tooltip(tooltip)),
    )
}

/// A 1Click deposit address, which also identifies the swap on NEAR Intents' explorer.
fn near_deposit_row(address: Address) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(address_row(
            "Deposit address",
            address,
            "Copy deposit address",
        ))
        .child(
            app_muted_text("Search it on explorer.near-intents.org to follow the swap on NEAR.")
                .whitespace_normal(),
        )
}

/// A deposit address in full, so it can be read out or checked character by character. It
/// wraps inside its box, and its copy control stays at the trailing edge.
fn deposit_address_box(address: Address) -> gpui::Div {
    let address = address.to_checksum(None);
    let copy_id = SharedString::from(format!("swap-deposit-address-{address}-copy"));
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text("Deposit address"))
        .child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .bg(rgb(theme::SETTINGS_INPUT_SURFACE))
                .debug_selector(|| "swap-detail-deposit-address".into())
                .child(
                    app_strong_text(address.clone())
                        .flex_1()
                        .min_w_0()
                        .font_family(theme::APP_MONO_FONT_FAMILY)
                        .whitespace_normal(),
                )
                .child(
                    div().flex_none().child(
                        clipboard_with_toast(copy_id, address).tooltip("Copy deposit address"),
                    ),
                ),
        )
}

/// An amount and a muted note after it, wrapping to the trailing edge when narrow.
pub(super) fn amount_with_note(amount: String, note: String) -> gpui::Div {
    div()
        .min_w_0()
        .flex()
        .flex_wrap()
        .justify_end()
        .gap_1()
        .child(app_text(amount))
        .when(!note.is_empty(), |value| value.child(app_muted_text(note)))
}

/// One fact, laid out like [`hash_row`].
pub(super) fn fact_row(label: impl Into<SharedString>, value: impl gpui::IntoElement) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .child(app_muted_text(label).flex_none())
        .child(value)
}

/// A swap's destination network as a fact's value, its mark before its name, at the size of
/// the Settled by mark. A `private` delivery's destination is the private balance there.
pub(super) fn destination_value(chain_id: u64, private: bool) -> gpui::Div {
    let network = network_mention(chain_id, px(16.0).into());
    if private {
        app_text("Private balance on")
            .flex()
            .items_center()
            .gap_1()
            .child(network)
    } else {
        network
            .text_size(theme::APP_TEXT_SIZE)
            .line_height(gpui::relative(theme::APP_TEXT_LINE_HEIGHT))
    }
}

/// Attempts of this pair on the swap's stealth account that ended without a fill before the
/// attempt the swap follows. The attempt whose bridge deposit delivered another attempt's
/// trade isn't one, nor an order that still competes with the followed one or that its trade
/// left unfilled, which the Orders section lists with its own status.
fn earlier_attempts_note(attempts: &SwapAttempts<'_>) -> Option<String> {
    let earlier = attempts
        .attempts
        .iter()
        .take_while(|order| order.uid() != attempts.outcome.uid())
        .filter(|order| attempts.ended_earlier(order))
        .collect::<Vec<_>>();
    let expired = |order: &SwapOrderRecord| {
        let observed = order.observations();
        observed.expired.is_some()
            || observed
                .pre_hook_dead
                .is_some_and(|death| death.cause == SwapPreHookDeathCause::Expired)
    };
    match earlier.as_slice() {
        [] => None,
        [order] if expired(order) => Some(format!(
            "1 earlier attempt expired at {} without a fill.",
            local_time_label(u64::from(order.valid_to()))
        )),
        [_] => Some("1 earlier attempt ended without a fill.".into()),
        orders if orders.iter().all(|order| expired(order)) => Some(format!(
            "{} earlier attempts expired without a fill.",
            orders.len()
        )),
        orders => Some(format!(
            "{} earlier attempts ended without a fill.",
            orders.len()
        )),
    }
}

/// A swap step and its sub-steps for the shared stepper. A sub-step names its network and its
/// stealth account, with a control that copies the account's address, and ends with its block
/// or what it waits for. A step that carries its own block ends with it. The order step shows
/// its order ID under its detail, and a step that carries a transaction its hash, each
/// shortened with a control that copies it in full. `action` is the step's own action, shown
/// under those.
pub(super) fn progress_group(
    step: &SwapStep,
    detail: String,
    id: String,
    action: Option<gpui::AnyElement>,
) -> SubmissionProgressGroup {
    let substeps = step
        .children
        .iter()
        .enumerate()
        .map(|(index, child)| {
            let key = child.account.map_or_else(
                || index.to_string(),
                |account| account.address.to_checksum(None),
            );
            SubmissionProgressSubstep {
                id: SharedString::from(format!("{id}-{key}")),
                label: child.label.clone(),
                status: child.status,
                content: child.account.map(|account| {
                    stealth_account("swap-step-account", account).into_any_element()
                }),
                outcome: child.detail.clone(),
            }
        })
        .collect();
    let id_row = |selector: &'static str,
                  label: &'static str,
                  hash: String,
                  copy: &str,
                  tooltip: &'static str| {
        let copy_id = SharedString::from(format!("{id}-{copy}-copy"));
        div()
            .flex()
            .items_center()
            .gap_1()
            .debug_selector(move || selector.into())
            .child(app_muted_text(label).flex_none())
            .child(app_muted_text(short_hash(&hash)).font_family(theme::APP_MONO_FONT_FAMILY))
            .child(clipboard_with_toast(copy_id, hash).tooltip(tooltip))
            .into_any_element()
    };
    let mut rows = [
        step.order.map(|order| {
            id_row(
                "swap-step-order",
                "Order ID",
                order.0.to_string(),
                "order",
                "Copy order ID",
            )
        }),
        step.transaction.map(|transaction| {
            id_row(
                "swap-step-transaction",
                "Tx hash",
                transaction.to_string(),
                "transaction",
                "Copy transaction hash",
            )
        }),
        // A non-EVM network's transaction in its own form, with no EVM prefix or link.
        step.destination_transaction.clone().map(|transaction| {
            let copy_id = SharedString::from(format!("{id}-destination-transaction-copy"));
            div()
                .flex()
                .items_center()
                .gap_1()
                .debug_selector(|| "swap-step-destination-transaction".into())
                .child(app_muted_text("Transaction").flex_none())
                .child(
                    app_muted_text(railgun_ui::short_non_evm(&transaction))
                        .font_family(theme::APP_MONO_FONT_FAMILY),
                )
                .child(clipboard_with_toast(copy_id, transaction).tooltip("Copy transaction"))
                .into_any_element()
        }),
        action,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let action = if rows.len() > 1 {
        Some(
            div()
                .flex()
                .flex_col()
                .items_start()
                .gap_1()
                .children(rows)
                .into_any_element(),
        )
    } else {
        rows.pop()
    };
    SubmissionProgressGroup {
        step: SubmissionProgressStep {
            action,
            ..progress_step(step, detail, id)
        },
        outcome: step.block.map_or_else(String::new, block_label),
        substeps,
    }
}

/// A block number as a step or a fact shows it, such as "block 402,118,977".
pub(super) fn block_label(block: u64) -> String {
    format!(
        "block {}",
        railgun_ui::format_token_amount(U256::from(block), 0)
    )
}

/// A stealth account's number and short address, with a control that copies the address in
/// full. `id` tells the account's places on one detail apart.
pub(super) fn stealth_account(id: &'static str, account: SwapStepAccount) -> gpui::Div {
    let short = short_receiver(account.address);
    let address = account.address.to_checksum(None);
    let copy_id = SharedString::from(format!("{id}-{address}-copy"));
    div()
        .min_w_0()
        .flex()
        .items_center()
        .gap_1()
        .child(
            app_muted_text(
                account
                    .index
                    .map_or_else(|| short.clone(), |index| format!("#{index} · {short}")),
            )
            .font_family(theme::APP_MONO_FONT_FAMILY),
        )
        .child(clipboard_with_toast(copy_id, address).tooltip("Copy stealth account address"))
}

/// The detail of a swap whose preparation was cancelled in this session: each stealth account
/// with a control that copies its address, and what the cancellation left of it. The footer
/// only closes.
fn render_cancelled_preparation(
    cancelled: &CancelledPreparation,
    source_available: bool,
    cx: &Context<'_, PrivateSwapsView>,
) -> (gpui::Div, Option<gpui::Div>) {
    let accounts = cancelled.accounts.iter().map(|account| {
        let (role, selector) = match account.role {
            CancelledAccountRole::Source => ("Source", "swap-cancelled-source"),
            CancelledAccountRole::Destination => ("Destination", "swap-cancelled-destination"),
        };
        div()
            .debug_selector(move || selector.into())
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(app_text(format!("{role} · {}", account.network)).flex_none())
                    .children(
                        account
                            .account
                            .map(|shown| stealth_account("swap-cancelled-account", shown)),
                    ),
            )
            .child(app_muted_text(account.outcome(source_available)).whitespace_normal())
    });
    let body = div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(app_strong_text("Preparation cancelled"))
                .child(app_muted_text("No order was placed for this swap.").whitespace_normal()),
        )
        .children(accounts);
    let footer = div()
        .w_full()
        .flex()
        .items_center()
        .child(div().flex_1())
        .child(
            app_button("swap-progress-close", "Close")
                .small()
                .flex_none()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.close_swap_dialog(window, cx);
                })),
        );
    (body, Some(footer))
}

/// The steps of one account's setup. With `stages`, this session's stages of its broadcaster
/// request come first. Without them one step stands for the sending and claims no stage's
/// result. The setup's confirmation follows either, and each step's ID names its row. `retry`
/// is the setup's own action, which sits on the step that failed, or else on the first step
/// that runs or hasn't started.
fn setup_steps(
    operation: ExecutorOperationId,
    setup: &SetupColumn,
    stages: Option<&SetupProgress>,
    retry: Option<gpui::AnyElement>,
) -> Vec<SubmissionProgressStep> {
    use PublicActionStepStatus::{Done, Error, NotStarted, Pending};
    let side = match setup.side {
        SetupSide::Origin => "origin",
        SetupSide::Destination => "destination",
    };
    // The stepper's body hides a detail equal to its label.
    let step = |label: &str, (status, detail): (PublicActionStepStatus, String), row: &str| {
        SubmissionProgressStep {
            detail: if detail.is_empty() {
                label.to_owned()
            } else {
                detail
            },
            label: label.to_owned(),
            status,
            error_copy_id: format!("swap-setup-{}-{side}-{row}", operation.opaque_id()).into(),
            action: None,
        }
    };
    let waiting = || SETUP_AWAITS_INCLUSION.to_owned();
    let confirming = || (Pending, setup.confirmation.clone().unwrap_or_else(waiting));
    // What the swap's own steps say of a setup that ended it.
    let outcome = || (setup.step.status, setup.step.detail.clone());
    let done = || (Done, String::new());
    let (mut steps, confirmation) = if let Some(stages) = stages {
        let now = std::time::Instant::now();
        let steps = stages
            .steps()
            .into_iter()
            .map(|stage| {
                let detail = stage
                    .message
                    .as_deref()
                    .map(str::to_owned)
                    .or_else(|| stages.wait_detail(&stage, now))
                    .unwrap_or_else(|| {
                        private_broadcaster_stage_detail(stage.stage, stage.status, false)
                            .to_owned()
                    });
                step(
                    private_broadcaster_stage_label(stage.stage, false),
                    (stage.status, detail),
                    private_broadcaster_stage_id(stage.stage),
                )
            })
            .collect::<Vec<_>>();
        let submitted = stages.end() == Some(SetupProgressEnd::Submitted);
        let confirmation = match setup.progress {
            SwapSetupProgress::Failed => outcome(),
            SwapSetupProgress::Done => done(),
            _ if submitted || setup.confirmation.is_some() => confirming(),
            _ => (NotStarted, waiting()),
        };
        (steps, confirmation)
    } else {
        let (sent, confirmation) = match setup.progress {
            SwapSetupProgress::Pending => (done(), confirming()),
            SwapSetupProgress::Done => (done(), done()),
            SwapSetupProgress::Failed => (outcome(), (NotStarted, String::new())),
            SwapSetupProgress::NotSent
            | SwapSetupProgress::NetworkLoading
            | SwapSetupProgress::Submitting => (outcome(), (NotStarted, waiting())),
        };
        // Only a setup the records show as sent is named so.
        let label = match setup.progress {
            SwapSetupProgress::Pending | SwapSetupProgress::Done => "Setup sent",
            _ => "Set up stealth account",
        };
        (vec![step(label, sent, "sent")], confirmation)
    };
    // A setup that ended with nothing sent in this session has no confirmation to wait for.
    if stages.is_some() || setup.progress != SwapSetupProgress::Failed {
        steps.push(step("Confirming setup", confirmation, "confirm"));
    }
    let at = |status: PublicActionStepStatus| steps.iter().position(|step| step.status == status);
    let row = at(Error).or_else(|| at(Pending)).or_else(|| at(NotStarted));
    if let Some(row) = row {
        steps[row].action = retry;
    }
    steps
}

/// A swap step for the shared stepper, whose body hides a detail equal to its label.
fn progress_step(step: &SwapStep, detail: String, error_copy_id: String) -> SubmissionProgressStep {
    SubmissionProgressStep {
        detail: if detail.is_empty() {
            step.label.clone()
        } else {
            detail
        },
        label: step.label.clone(),
        status: step.status,
        error_copy_id: SharedString::from(error_copy_id),
        action: None,
    }
}

/// The swap's error under its detail.
fn progress_error(error: String) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .debug_selector(|| "swap-progress-error".into())
        .child(Alert::error("swap-progress-error", error).small().min_w_0())
}

/// A completed swap's title says so, so its outcome replaces its steps. Steps that still wait
/// or need attention stay above the outcome.
fn shows_steps(stage: SwapStage, outcome: bool) -> bool {
    !outcome || stage != SwapStage::Order(SwapOrderState::Done)
}

/// One outcome value, laid out like [`hash_row`], with an optional note under the value.
fn outcome_row(label: &'static str, value: String, note: Option<String>) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_start()
        .justify_between()
        .gap_2()
        .child(app_muted_text(label).flex_none())
        .child(
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .items_end()
                .child(app_text(value).whitespace_normal())
                .children(note.map(|note| app_muted_text(note).text_xs().whitespace_normal())),
        )
}

/// One side of a traded swap's amounts: the token's icon and amount, and a muted line under it.
/// The trailing side aligns to the end.
fn outcome_side(
    icon: Option<WalletIconSource>,
    amount: String,
    note: String,
    trailing: bool,
) -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .when(trailing, gpui::Styled::items_end)
        .child(
            div()
                .max_w_full()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .children(icon.map(|icon| img(icon).size(px(20.0)).rounded_full().flex_none()))
                .child(
                    app_strong_text(amount)
                        .min_w_0()
                        .text_size(theme::BALANCE_TEXT_SIZE)
                        .font_weight(FontWeight::SEMIBOLD)
                        .whitespace_normal()
                        .when(trailing, gpui::Styled::text_right),
                ),
        )
        .child(
            app_muted_text(note)
                .text_xs()
                .whitespace_normal()
                .when(trailing, gpui::Styled::text_right),
        )
}

/// A disclosure row of a traded swap's outcome: its label, an optional summary and a chevron.
/// The caller adds the content while it's open.
fn outcome_disclosure(
    id: &'static str,
    label: &'static str,
    summary: Option<gpui::Div>,
    open: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Collapsible {
    let toggle = format!(
        "{} {}",
        if open { "Hide" } else { "Show" },
        label.to_lowercase()
    );
    Collapsible::new()
        .open(open)
        .w_full()
        .min_w_0()
        .gap_2()
        .child(
            app_button_base(id)
                .ghost()
                .w_full()
                .min_w_0()
                .h_auto()
                .px_0()
                .py_0()
                .accessibility_label(toggle.clone())
                .tooltip(toggle)
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(app_muted_text(label).flex_1().min_w_0())
                        .children(summary.map(gpui::Styled::flex_none))
                        .child(
                            Icon::new(if open {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall()
                            .flex_none()
                            .text_color(rgb(theme::TEXT_MUTED)),
                        ),
                )
                .on_click(on_toggle),
        )
}

/// An open disclosure's rows, indented behind a rule.
fn outcome_nested(rows: Vec<gpui::Div>) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .pl_3()
        .border_l_2()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .children(rows)
}

const fn progress_note(stage: SwapStage) -> Option<&'static str> {
    match stage {
        SwapStage::SubmissionPending | SwapStage::SubmissionRejected => Some(
            "The signed order stays reserved until its expiry is final. A submission timeout does not prove that the orderbook rejected it.",
        ),
        SwapStage::SetupSubmitting
        | SwapStage::SetupPending
        | SwapStage::Order(
            SwapOrderState::Open
            | SwapOrderState::Traded
            | SwapOrderState::Bridging
            | SwapOrderState::PreHookOnly { expired: false },
        ) => Some(
            "You can close this. The swap keeps running and its status stays on the Private tab.",
        ),
        SwapStage::Order(SwapOrderState::AttemptEnded(_)) => Some(
            "Retrying reuses this stealth account, so there's no second setup cost. You review a new quote first.",
        ),
        SwapStage::Order(
            SwapOrderState::PreHookOnly { expired: true } | SwapOrderState::NotDelivered,
        ) => Some(
            "Recovering shields the funds back to your private balance. It costs the shield fee and a broadcaster fee.",
        ),
        SwapStage::Ready => {
            Some("The stealth account is set up. Review a quote to place the order.")
        }
        SwapStage::Approved => Some(
            "The stealth account is set up. Placing the order checks the fee, the price and your minimum again. If one changed, you review the swap again.",
        ),
        SwapStage::SetupRetired => {
            Some("Retry with a new account. The old one stays in Stealth accounts.")
        }
        SwapStage::SetupNotSent | SwapStage::SetupFailed => Some(
            "Continue to send the setup again with the same stealth account. Nothing was unshielded.",
        ),
        // A Bridge swap's facts explain its refund, what needs attention, or the recovery of
        // held proceeds on the destination network. A superseded order's step names the fill.
        SwapStage::Order(
            SwapOrderState::Done
            | SwapOrderState::Refunding
            | SwapOrderState::NeedsAttention
            | SwapOrderState::HeldOnDestination
            | SwapOrderState::Superseded,
        )
        | SwapStage::Recovered => None,
    }
}
