use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Pixels, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder as _, px, rems, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable, WindowExt, button::ButtonVariants, tooltip::Tooltip,
};
use tokio::sync::watch;
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button, app_button_base, app_muted_text, app_strong_text};
use ui::dialog::WalletDialogExt as _;
use ui::theme;
use wallet_ops::{
    DesktopSelfBroadcastResult, PublicBroadcasterCostEstimate, PublicBroadcasterResultKind,
    PublicBroadcasterSubmissionResult, SelfBroadcastAttemptInfo, SelfBroadcastCommand,
    SelfBroadcastCommandKind, SelfBroadcastCommandSender, SelfBroadcastGasFeeSelection,
    SponsoredSelfBroadcastCommand, SponsoredSelfBroadcastSessionOutcome, SubmissionRouteProgress,
    SubmissionRouteSender, TransactionGenerationStage, WalletNetworkMode,
    self_broadcast_replacement_bumped_fee,
};

use super::broadcaster_picker::BroadcasterChoice;
use super::gas_fee::{GasRetryInputs, format_gwei};
use super::private_action::{
    delivery_element_id, fee_editor::BroadcasterFeeRetry, private_action_title_row,
};
use super::public_action::{
    ProgressDialogCloseBehavior, ProgressFooterAction, PublicActionStepStatus,
    progress_footer_action,
};
use super::public_broadcaster_cost::{
    PrivateBroadcasterProgressContext, PublicBroadcasterCostDisplay, cost_estimate_detail_text,
    private_broadcaster_context_row, render_private_broadcaster_progress_context,
    render_public_broadcaster_tx_hash_row,
};
use super::spend_authorization::spend_authorization_recipient_display;
use super::submission_progress::{SubmissionProgressStep, render_submission_progress_stepper};
use super::submission_route::{SubmissionRouteCard, render_submission_route_indicator};
use super::utxo::short_hash;
use super::{
    DeliveryFormKind, PRIVATE_BROADCASTER_PROGRESS_DIALOG_WIDTH, UnshieldAssetKey, WalletRoot,
    app_panel, app_status_tag, dialog_max_height, format_native_token_amount_for_display,
    format_recipient_amount_with_native_top_up, format_report_chain,
    secondary_dialog_content_width,
};

const SPONSORED_SHUTDOWN_ABORT_GRACE: Duration = Duration::from_secs(20);
const SPONSORED_CANCEL_ABORT_GRACE: Duration = Duration::from_secs(90);

use crate::assets::{RailgunActionIcon, WalletIconSource};

mod gateway;
#[cfg(test)]
pub(super) use gateway::{gateway_self_broadcast_result, gateway_self_broadcast_status};
mod broadcaster_retries;
mod progress;
mod progress_lifecycle;
mod progress_render;
mod progress_updates;
mod types;

pub(in crate::root) use progress::broadcaster_wait_detail;
pub(super) use progress::private_broadcaster_route_card;
pub(super) use progress::{
    apply_private_broadcaster_progress_stage, end_private_broadcaster_progress_steps,
    ensure_self_broadcast_unshield_progress_stage,
    fail_private_broadcaster_progress_steps_at_stage,
    finish_private_broadcaster_progress_steps_at_stage,
    finish_private_self_broadcast_progress_steps_at_stage,
    mark_private_broadcaster_active_step_stopped, private_broadcaster_closed_active_progress,
    private_broadcaster_progress_dialog_close_behavior, private_broadcaster_progress_footer_action,
    private_broadcaster_progress_is_terminal, private_broadcaster_progress_steps,
    private_broadcaster_stage_detail, private_broadcaster_stage_id,
    private_broadcaster_stage_label, private_progress_stage_disables_stop,
    private_submission_discard_attempt_available, self_broadcast_progress_steps,
    self_broadcast_step_retry_kind, sponsored_stop_uses_session_command,
};
use progress::{
    ensure_private_broadcaster_progress_stage, private_broadcaster_progress_stop_available,
    private_broadcaster_retry_button_id, private_broadcaster_step_detail,
    public_broadcaster_waiting_can_stop,
};
#[cfg(test)]
pub(super) use progress::{
    finish_private_broadcaster_progress_steps, format_public_broadcaster_wait_remaining,
    public_broadcaster_wait_status_detail,
};
#[cfg(test)]
pub(super) use progress::{
    private_broadcaster_closed_active_stage, private_broadcaster_progress_is_successful,
};
use types::private_submission_dialog_title_action;
pub(super) use types::{
    PrivateBroadcasterClosedActiveProgress, PrivateBroadcasterProgressState,
    PrivateBroadcasterProgressStepState, PrivateSubmissionProgressFlow,
    SELF_BROADCAST_GAS_RETRY_DIALOG_WIDTH, SelfBroadcastGasRetryKind,
};

pub(super) struct SelfBroadcastGasRetryDialogContent {
    root: Entity<WalletRoot>,
    kind: DeliveryFormKind,
    key: UnshieldAssetKey,
    generation_id: u64,
    retry_kind: SelfBroadcastGasRetryKind,
    gas_inputs: GasRetryInputs,
    error: Option<Arc<str>>,
}

impl SelfBroadcastGasRetryDialogContent {
    pub(in crate::root) fn submit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let (max_fee, max_tip) = match self.gas_inputs.parse(cx) {
            Ok(values) => values,
            Err(error) => {
                self.error = Some(Arc::from(error));
                cx.notify();
                return;
            }
        };
        self.root.update(cx, |root, cx| {
            root.submit_self_broadcast_gas_retry(
                self.kind,
                self.key,
                self.generation_id,
                self.retry_kind,
                max_fee,
                max_tip,
                cx,
            );
        });
        window.close_dialog(cx);
    }

    fn new(
        root: Entity<WalletRoot>,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        retry_kind: SelfBroadcastGasRetryKind,
        initial_max_fee_per_gas: u128,
        initial_max_priority_fee_per_gas: u128,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let gas_inputs = GasRetryInputs::new(
            initial_max_fee_per_gas,
            initial_max_priority_fee_per_gas,
            window,
            cx,
        );
        cx.observe(&root, |_this, _root, cx| cx.notify()).detach();
        gas_inputs.subscribe_clear_error(cx, |this, cx| {
            this.error = None;
            cx.notify();
        });
        Self {
            root,
            kind,
            key,
            generation_id,
            retry_kind,
            gas_inputs,
            error: None,
        }
    }
}

impl gpui::Render for SelfBroadcastGasRetryDialogContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let title = match self.retry_kind {
            SelfBroadcastGasRetryKind::RetryStep => "Retry step",
            SelfBroadcastGasRetryKind::RetryEstimate => "Retry with custom gas",
            SelfBroadcastGasRetryKind::SpeedUp => "Speed up transaction",
        };
        let detail = match self.retry_kind {
            SelfBroadcastGasRetryKind::RetryStep => {
                "Retry signing and sending this self-broadcast step with the current gas fee values."
            }
            SelfBroadcastGasRetryKind::RetryEstimate => {
                "Retry gas estimation and signing using these EIP-1559 fee values."
            }
            SelfBroadcastGasRetryKind::SpeedUp => {
                "Uses the same nonce to replace the pending transaction. Values are prefilled +12.5%."
            }
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(app_strong_text(title))
            .child(app_muted_text(detail).whitespace_normal())
            .child(self.gas_inputs.render_fields())
            .when_some(self.error.as_ref(), |this, error| {
                this.child(app_muted_text(error.to_string()).text_color(rgb(theme::DANGER)))
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("self-broadcast-gas-retry-cancel", "Cancel")
                            .flex_none()
                            .on_click(move |_event, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        app_button("self-broadcast-gas-retry-confirm", "Submit")
                            .primary()
                            .flex_none()
                            .on_click(cx.listener(|this, _event, window, cx| {
                                this.submit(window, cx);
                            })),
                    ),
            )
    }
}

/// What a submission's progress shows of its broadcaster's preference state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum BroadcasterPreferenceControl {
    Banned,
    Favorited,
    AddFavorite,
    Ban,
}

/// The preference indicator or control of a submission's broadcaster: Banned whenever it is
/// banned, the favorite state once it `submitted`, and Ban while the flow has `ban_offered`.
pub(in crate::root) const fn broadcaster_preference_control(
    banned: bool,
    favorite: bool,
    submitted: bool,
    ban_offered: bool,
) -> Option<BroadcasterPreferenceControl> {
    match (banned, submitted, favorite, ban_offered) {
        (true, ..) => Some(BroadcasterPreferenceControl::Banned),
        (false, true, true, _) => Some(BroadcasterPreferenceControl::Favorited),
        (false, true, false, _) => Some(BroadcasterPreferenceControl::AddFavorite),
        (false, false, _, true) => Some(BroadcasterPreferenceControl::Ban),
        (false, false, _, false) => None,
    }
}

/// Draw `control` for the broadcaster at `address`. Its buttons' IDs start with `id_prefix`,
/// and a click changes `root`'s preferences, then calls `on_change`.
pub(in crate::root) fn render_broadcaster_preference_control(
    control: BroadcasterPreferenceControl,
    id_prefix: SharedString,
    address: &str,
    root: gpui::WeakEntity<WalletRoot>,
    on_change: impl Fn(&mut App) + 'static,
) -> AnyElement {
    use ui::private_submission::OperationControl;

    let address = address.to_owned();
    match control {
        BroadcasterPreferenceControl::Banned => {
            render_broadcaster_preference_progress_chip("Banned", theme::DANGER, None)
                .into_any_element()
        }
        BroadcasterPreferenceControl::Favorited => render_broadcaster_preference_progress_chip(
            "Favorited",
            theme::WARNING,
            Some(IconName::Star),
        )
        .into_any_element(),
        BroadcasterPreferenceControl::AddFavorite => {
            app_button_base(SharedString::from(format!("{id_prefix}-favorite")))
                .outline()
                .xsmall()
                .flex_none()
                .icon(Icon::new(IconName::Star))
                .accessibility_label("Add to favorites")
                .tooltip(
                    "Save this broadcaster to your favorites so future transactions can prefer it.",
                )
                .on_click(move |_event, _window, cx| {
                    let _ = root.update(cx, |root, cx| {
                        root.add_favorite_broadcaster(&address, cx);
                    });
                    on_change(cx);
                })
                .into_any_element()
        }
        BroadcasterPreferenceControl::Ban => ui::private_submission::operation_controls(
            id_prefix,
            [OperationControl::Ban],
            move |_control, _, cx| {
                let _ = root.update(cx, |root, cx| {
                    root.add_banned_broadcaster(&address, cx);
                });
                on_change(cx);
            },
        )
        .flex_none()
        .into_any_element(),
    }
}

pub(in crate::root) fn cancel_private_broadcaster_progress(
    progress: &mut Option<PrivateBroadcasterProgressState>,
) -> Option<tokio::task::AbortHandle> {
    let mut progress = progress.take()?;
    if let Some(command_tx) = progress.sponsored_self_broadcast_command_tx.take() {
        let _ = command_tx.send(SponsoredSelfBroadcastCommand::Shutdown);
        let session_active = progress.steps.iter().any(|step| {
            step.status == PublicActionStepStatus::Pending
                && sponsored_stop_uses_session_command(step.stage)
        });
        let handle = progress.task_abort_handle.take();
        if !session_active {
            if let Some(handle) = handle {
                handle.abort();
            }
            return None;
        }
        return handle;
    }
    if let Some(handle) = progress.task_abort_handle.take() {
        handle.abort();
    }
    None
}

pub(in crate::root) fn request_sponsored_session_cancel(
    progress: &mut PrivateBroadcasterProgressState,
) -> bool {
    if let Some(command_tx) = progress.sponsored_self_broadcast_command_tx.as_ref() {
        let _ = command_tx.send(SponsoredSelfBroadcastCommand::Cancel);
        true
    } else if let Some(handle) = progress.task_abort_handle.take() {
        handle.abort();
        false
    } else {
        false
    }
}

pub(in crate::root) fn cancel_public_broadcaster_tasks(
    handles: &mut Vec<tokio::task::AbortHandle>,
) {
    for handle in handles.drain(..) {
        handle.abort();
    }
}

pub(in crate::root) fn spawn_sponsored_abort_watchdog(
    runtime: &tokio::runtime::Handle,
    handle: tokio::task::AbortHandle,
    grace: Duration,
) {
    drop(runtime.spawn(async move {
        tokio::time::sleep(grace).await;
        if !handle.is_finished() {
            handle.abort();
        }
    }));
}

fn public_broadcaster_progress_address(progress: &PrivateBroadcasterProgressState) -> Option<&str> {
    progress.result.as_ref().map_or_else(
        || {
            progress
                .estimate
                .as_ref()
                .map(|estimate| estimate.broadcaster.railgun_address.as_ref())
        },
        |result| Some(result.broadcaster.railgun_address.as_ref()),
    )
}

pub(in crate::root) fn render_broadcaster_preference_progress_chip(
    label: &'static str,
    color: u32,
    icon: Option<IconName>,
) -> impl IntoElement {
    app_status_tag(label, color)
        .flex_none()
        .gap_1()
        .children(icon.map(|icon| Icon::new(icon).size(rems(0.8125))))
}

fn render_pending_public_broadcaster_progress_context(
    progress: &PrivateBroadcasterProgressState,
) -> gpui::Div {
    app_panel(theme::SURFACE_ELEVATED, theme::BORDER)
        .child(app_strong_text("Transaction context"))
        .child(private_broadcaster_context_row(
            "Broadcaster",
            "Selecting during preparation".to_string(),
        ))
        .child(private_broadcaster_context_row(
            "Recipient",
            spend_authorization_recipient_display(progress.recipient.as_ref()),
        ))
        .child(cost_estimate_detail_text(
            "Fee and gas cost will appear after the transaction fee is calculated.",
        ))
}

fn private_broadcaster_copy_action(id: String, value: String, tooltip: &'static str) -> AnyElement {
    div()
        .id(SharedString::from(format!("{id}-action")))
        .flex_none()
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        .child(clipboard_with_toast(SharedString::from(id), value))
        .into_any_element()
}

fn self_broadcast_progress_context_rows(
    progress: &PrivateBroadcasterProgressState,
) -> Vec<ui::private_action::DisplayRow> {
    let mut values = vec![
        (
            if progress.sponsored_funding {
                "Transaction signer"
            } else {
                "Gas payer"
            },
            progress
                .gas_payer
                .as_deref()
                .unwrap_or("Selected Public account")
                .to_owned(),
        ),
        ("Recipient", progress.recipient.to_string()),
    ];
    if let Some(result) = &progress.self_broadcast_result {
        values.extend(self_broadcast_composite_output_rows(progress, result));
        values.extend([
            (
                "Max fee",
                format!("{} gwei", format_gwei(result.max_fee_per_gas)),
            ),
            (
                "Max tip",
                format!("{} gwei", format_gwei(result.max_priority_fee_per_gas)),
            ),
            (
                "Estimated gas cost",
                format_native_token_amount_for_display(
                    result.chain_id,
                    result.estimated_native_gas_cost,
                ),
            ),
            (
                "Receipt",
                match result.tx.receipt() {
                    Some(receipt) if receipt.status => "confirmed",
                    Some(_) => "reverted",
                    None => "inclusion unknown",
                }
                .into(),
            ),
        ]);
        if let Some(receipt) = result.tx.receipt() {
            values.push(("Block", receipt.block_number.to_string()));
        }
        if matches!(
            result.tx,
            wallet_ops::SelfBroadcastTxOutcome::ExecutorReceipt { .. }
        ) {
            // The receipt tells a revert at the time. It is recorded nowhere.
            let execution = match result.tx.execution_status() {
                Some(true) => "confirmed",
                Some(false) => "reverted",
                None => "not confirmed",
            };
            values.push(("Execution", execution.into()));
        }
    } else if let Some(output) = &progress.recipient_output {
        values.push(("Recipient receives", output.to_string()));
    }
    if let Some(SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(receipt)) =
        &progress.sponsored_self_broadcast_outcome
    {
        values.extend([
            (
                "Receipt",
                if receipt.status {
                    "confirmed"
                } else {
                    "reverted"
                }
                .into(),
            ),
            ("Transaction hash", receipt.tx_hash.clone()),
            ("Block", receipt.block_number.to_string()),
        ]);
    }
    values
        .into_iter()
        .map(|(label, value)| ui::private_action::DisplayRow {
            label: label.into(),
            value,
            suffix: None,
        })
        .collect()
}

fn render_self_broadcast_progress_context(progress: &PrivateBroadcasterProgressState) -> gpui::Div {
    let rows = self_broadcast_progress_context_rows(progress)
        .into_iter()
        .map(|mut row| {
            let action = match row.label.as_str() {
                "Recipient" => {
                    let action = private_broadcaster_copy_action(
                        format!(
                            "private-self-broadcast-recipient-copy-{}",
                            progress.generation_id
                        ),
                        row.value.clone(),
                        "Copy recipient",
                    );
                    row.value = spend_authorization_recipient_display(&row.value);
                    Some(action)
                }
                "Transaction hash" => {
                    let action = private_broadcaster_copy_action(
                        format!(
                            "private-self-broadcast-tx-hash-copy-{}",
                            progress.generation_id
                        ),
                        row.value.clone(),
                        "Copy transaction hash",
                    );
                    row.value = short_hash(&row.value);
                    Some(action)
                }
                _ => None,
            };
            (row, action)
        })
        .collect();
    ui::private_submission::transaction_context_with_actions(rows)
        .p(px(12.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::BORDER))
}

pub(in crate::root) fn self_broadcast_composite_output_rows(
    progress: &PrivateBroadcasterProgressState,
    result: &DesktopSelfBroadcastResult,
) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    if let Some(output) = progress.recipient_output.as_ref() {
        let output = result.native_top_up.as_ref().map_or_else(
            || output.to_string(),
            |top_up| {
                format_recipient_amount_with_native_top_up(
                    output.as_ref(),
                    result.chain_id,
                    top_up.native_amount,
                )
            },
        );
        rows.push(("Recipient receives", output));
    } else if let Some(top_up) = result.native_top_up.as_ref() {
        rows.push((
            "Recipient receives",
            format!(
                "{} (gas top-up)",
                format_native_token_amount_for_display(result.chain_id, top_up.native_amount)
            ),
        ));
    }
    rows
}

fn render_private_broadcaster_progress_footer(
    root: Entity<WalletRoot>,
    progress: &PrivateBroadcasterProgressState,
) -> gpui::Div {
    let action = progress_footer_action(
        private_broadcaster_progress_stop_available(progress),
        private_broadcaster_progress_is_terminal(progress),
    );
    if matches!(action, ProgressFooterAction::Stop) {
        use ui::private_submission::OperationControl;
        let control = if progress.sponsored_funding {
            OperationControl::StopRetries
        } else if public_broadcaster_waiting_can_stop(progress) && !progress.stop_available {
            OperationControl::StopWaiting
        } else {
            OperationControl::Stop
        };
        return ui::private_submission::operation_controls(
            delivery_element_id(progress.key, progress.kind, "progress-controls"),
            [control],
            move |_, _, cx| root.update(cx, WalletRoot::stop_private_broadcaster_progress),
        )
        .w_full()
        .justify_end();
    }
    let button_root = root;
    let (id_suffix, label) = match action {
        ProgressFooterAction::Stop => (
            "progress-stop",
            if public_broadcaster_waiting_can_stop(progress) && !progress.stop_available {
                "Stop waiting"
            } else if progress.sponsored_funding {
                "Stop retries"
            } else {
                "Stop"
            },
        ),
        ProgressFooterAction::Close => ("progress-close", "Close"),
    };
    let button = app_button(
        delivery_element_id(progress.key, progress.kind, id_suffix),
        label,
    )
    .small()
    .flex_none();
    let button = match action {
        ProgressFooterAction::Stop => button.danger().icon(Icon::new(RailgunActionIcon::Square)),
        ProgressFooterAction::Close => button.outline(),
    };
    div()
        .w_full()
        .flex()
        .justify_end()
        .pt(px(2.0))
        .child(button.on_click(move |_event, window, cx| {
            button_root.update(cx, |root, cx| match action {
                ProgressFooterAction::Stop => root.stop_private_broadcaster_progress(cx),
                ProgressFooterAction::Close => {
                    root.close_private_broadcaster_progress_dialog(window, cx);
                }
            });
        }))
}

/// `refusal_actions` is `Some(offer_another_broadcaster)` when a broadcaster refusal can be retried.
/// `input_pin_override` offers to continue a submission the form's note pin stopped.
pub(super) fn render_private_broadcaster_progress_stepper(
    root: &Entity<WalletRoot>,
    progress: &PrivateBroadcasterProgressState,
    refusal_actions: Option<bool>,
    input_pin_override: bool,
) -> gpui::Div {
    render_submission_progress_stepper(progress.steps.iter().map(|step| {
        let detail = if step.status == PublicActionStepStatus::Error {
            step.message
                .as_deref()
                .unwrap_or("This broadcaster submission step failed.")
                .to_owned()
        } else {
            private_broadcaster_step_detail(progress, step, Instant::now())
        };
        SubmissionProgressStep {
            label: private_broadcaster_stage_label(step.stage, progress.requires_device_approval)
                .into(),
            detail,
            status: step.status,
            error_copy_id: format!(
                "wallet-private-broadcaster-{}-error-copy",
                private_broadcaster_stage_id(step.stage)
            )
            .into(),
            action: match refusal_actions {
                Some(another) if step.status == PublicActionStepStatus::Error => Some(
                    render_refused_broadcaster_step_action(root.clone(), progress, another),
                ),
                _ if input_pin_override && step.status == PublicActionStepStatus::Error => Some(
                    render_input_pin_conflict_step_action(root.clone(), progress),
                ),
                _ => render_self_broadcast_step_action(root.clone(), progress, step),
            },
        }
    }))
}

fn render_refused_broadcaster_step_action(
    root: Entity<WalletRoot>,
    progress: &PrivateBroadcasterProgressState,
    another_broadcaster: bool,
) -> gpui::AnyElement {
    let key = progress.key;
    let kind = progress.kind;
    let generation_id = progress.generation_id;
    let pending = progress.refusal_retry_pending;
    let custom_fee_root = root.clone();
    let buttons = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(
            app_button(
                delivery_element_id(key, kind, "progress-retry-custom-fee"),
                "Retry with custom fee…",
            )
            .small()
            .outline()
            .disabled(pending)
            .on_click(move |_event, window, cx| {
                custom_fee_root.update(cx, |root, cx| {
                    root.retry_public_broadcaster_with_custom_fee(
                        kind,
                        key,
                        generation_id,
                        window,
                        cx,
                    );
                });
            }),
        )
        .when(another_broadcaster, |this| {
            this.child(
                app_button(
                    delivery_element_id(key, kind, "progress-try-another-broadcaster"),
                    "Try another broadcaster",
                )
                .small()
                .outline()
                .disabled(pending)
                .on_click(move |_event, window, cx| {
                    root.update(cx, |root, cx| {
                        root.try_another_public_broadcaster(kind, key, generation_id, window, cx);
                    });
                }),
            )
        });
    div()
        .pt(px(4.0))
        .flex()
        .flex_col()
        .items_start()
        .gap_1()
        .child(buttons)
        .when(pending, |this| {
            this.child(app_muted_text("Getting a new fee estimate…").whitespace_normal())
        })
        .when_some(progress.refusal_retry_error.as_deref(), |this, error| {
            this.child(
                app_muted_text(format!("Retry failed: {error}"))
                    .text_color(rgb(theme::DANGER))
                    .whitespace_normal(),
            )
        })
        .into_any_element()
}

fn render_input_pin_conflict_step_action(
    root: Entity<WalletRoot>,
    progress: &PrivateBroadcasterProgressState,
) -> gpui::AnyElement {
    let key = progress.key;
    let kind = progress.kind;
    let generation_id = progress.generation_id;
    div()
        .pt(px(4.0))
        .child(
            app_button(
                delivery_element_id(key, kind, "progress-continue-despite-pin"),
                "Continue anyway…",
            )
            .small()
            .outline()
            .on_click(move |_event, window, cx| {
                root.update(cx, |root, cx| {
                    root.continue_despite_input_pin(kind, key, generation_id, window, cx);
                });
            }),
        )
        .into_any_element()
}

fn render_self_broadcast_step_action(
    root: Entity<WalletRoot>,
    progress: &PrivateBroadcasterProgressState,
    step: &PrivateBroadcasterProgressStepState,
) -> Option<gpui::AnyElement> {
    if progress.flow != PrivateSubmissionProgressFlow::SelfBroadcast
        || progress.self_broadcast_command_tx.is_none()
        || progress.stopped
        || progress.error.is_some()
        || progress.self_broadcast_result.is_some()
    {
        return None;
    }
    let retry_kind = self_broadcast_step_retry_kind(progress, step)?;
    let label = match retry_kind {
        SelfBroadcastGasRetryKind::RetryStep => "Retry step",
        SelfBroadcastGasRetryKind::RetryEstimate => "Retry with custom gas",
        SelfBroadcastGasRetryKind::SpeedUp => "Speed up transaction",
    };
    let key = progress.key;
    let kind = progress.kind;
    let generation_id = progress.generation_id;
    let mut action = div()
        .pt(px(4.0))
        .flex()
        .flex_col()
        .items_start()
        .gap_1()
        .child(
            app_button(
                delivery_element_id(key, kind, private_broadcaster_retry_button_id(retry_kind)),
                label,
            )
            .small()
            .outline()
            .on_click(move |_event, window, cx| {
                root.update(cx, |root, cx| {
                    if retry_kind == SelfBroadcastGasRetryKind::RetryStep {
                        root.submit_self_broadcast_step_retry(kind, key, generation_id, cx);
                    } else {
                        root.open_self_broadcast_gas_retry_dialog(
                            kind,
                            key,
                            generation_id,
                            retry_kind,
                            window,
                            cx,
                        );
                    }
                });
            }),
        );
    if let Some(error) = progress.self_broadcast_action_error.as_deref() {
        action = action.child(
            app_muted_text(format!("Last retry failed: {error}"))
                .text_color(rgb(theme::DANGER))
                .whitespace_normal(),
        );
    }
    Some(action.into_any_element())
}

/// The route card of the root's private submission progress, which the progress dialog's route
/// chip shows on hover.
fn private_broadcaster_root_route_card(
    root: &WalletRoot,
    _cx: &App,
) -> Option<SubmissionRouteCard> {
    root.private_broadcaster_progress
        .as_ref()
        .and_then(private_broadcaster_route_card)
}

pub(super) fn render_private_broadcaster_status_notice(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    result: &PublicBroadcasterResultKind,
) -> gpui::Div {
    let (title, detail, border) = match result {
        PublicBroadcasterResultKind::Submitted { .. } => (
            "Submitted via public broadcaster",
            "Open the broadcaster status dialog for the transaction details.",
            theme::SUCCESS,
        ),
        PublicBroadcasterResultKind::Failed { .. } => (
            "Public broadcaster failed",
            "Open the broadcaster status dialog for the returned error.",
            theme::DANGER,
        ),
        PublicBroadcasterResultKind::TimedOut => (
            "Public broadcaster timed out",
            "Check history before submitting again. A new submission is sent from a new identity.",
            theme::WARNING,
        ),
    };
    render_private_broadcaster_status_notice_box(root, key, kind, title, detail, border, None)
}

pub(super) fn render_private_self_broadcast_status_notice(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    result: &DesktopSelfBroadcastResult,
) -> gpui::Div {
    let (title, detail, border) = match result.tx.execution_status() {
        Some(true) => (
            "Self-broadcast confirmed",
            "Open the self-broadcast status dialog for transaction details.",
            theme::SUCCESS,
        ),
        Some(_) => (
            "Self-broadcast reverted",
            "Open the self-broadcast status dialog for receipt details.",
            theme::DANGER,
        ),
        None if result.tx.receipt().is_some() => (
            "Execution not confirmed",
            "The receipt alone does not confirm the execution. Stealth accounts keeps this operation, where Check balances updates its result.",
            theme::WARNING,
        ),
        None => (
            "Transaction inclusion unknown",
            progress::SELF_BROADCAST_INCLUSION_UNOBSERVED_MESSAGE,
            theme::WARNING,
        ),
    };
    render_private_broadcaster_status_notice_box(root, key, kind, title, detail, border, None)
}

pub(super) fn render_private_submission_active_status_notice(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    active: &PrivateBroadcasterClosedActiveProgress,
) -> gpui::Div {
    let title = match (active.flow, active.step.status) {
        (PrivateSubmissionProgressFlow::PublicBroadcaster, PublicActionStepStatus::Error) => {
            "Public broadcaster needs attention"
        }
        (PrivateSubmissionProgressFlow::SelfBroadcast, PublicActionStepStatus::Error) => {
            "Self-broadcast needs attention"
        }
        (PrivateSubmissionProgressFlow::PublicBroadcaster, _) => "Public broadcaster in progress",
        (PrivateSubmissionProgressFlow::SelfBroadcast, _) => "Self-broadcast in progress",
    };
    let border = if active.step.status == PublicActionStepStatus::Error {
        theme::DANGER
    } else {
        theme::INFO
    };
    let discard_generation_id =
        private_submission_discard_attempt_available(active).then_some(active.generation_id);
    render_private_broadcaster_status_notice_box(
        root,
        key,
        kind,
        title,
        private_submission_active_status_detail(active),
        border,
        discard_generation_id,
    )
}

fn private_submission_active_status_detail(
    active: &PrivateBroadcasterClosedActiveProgress,
) -> String {
    let stage = active.step.stage;
    let detail = match active.step.status {
        PublicActionStepStatus::Error => active.step.message.as_deref().map_or_else(
            || "This private submission step failed.".to_string(),
            private_submission_error_summary,
        ),
        _ => private_broadcaster_stage_detail(
            stage,
            active.step.status,
            active.requires_device_approval,
        )
        .to_string(),
    };
    format!(
        "{}: {detail}",
        private_broadcaster_stage_label(stage, active.requires_device_approval)
    )
}

fn private_submission_error_summary(message: &str) -> String {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("cancel")
        || normalized.contains("rejected")
        || normalized.contains("denied")
    {
        return "Signing was cancelled.".to_string();
    }
    message.to_string()
}

fn render_private_broadcaster_status_notice_box(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    border: u32,
    discard_generation_id: Option<u64>,
) -> gpui::Div {
    let view_root = root.clone();
    let discard_root = root;
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .p(px(10.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(border))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_1()
                .child(app_strong_text(title))
                .child(app_muted_text(detail).whitespace_normal()),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .justify_end()
                .gap_2()
                .child(
                    app_button(
                        delivery_element_id(key, kind, "view-broadcaster-progress"),
                        "View status",
                    )
                    .outline()
                    .small()
                    .on_click(move |_event, window, cx| {
                        view_root.update(cx, |root, cx| {
                            root.show_private_broadcaster_progress_dialog(window, cx);
                        });
                    }),
                )
                .children(discard_generation_id.map(|generation_id| {
                    app_button(
                        delivery_element_id(key, kind, "discard-attempt"),
                        "Discard attempt",
                    )
                    .danger()
                    .small()
                    .on_click(move |_event, _window, cx| {
                        discard_root.update(cx, |root, cx| {
                            root.discard_private_submission_attempt(kind, key, generation_id, cx);
                        });
                    })
                })),
        )
}
