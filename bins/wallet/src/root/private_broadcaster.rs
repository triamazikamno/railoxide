use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Pixels, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Icon, IconName, Sizable, WindowExt, button::ButtonVariants, tooltip::Tooltip,
};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button, app_button_base, app_muted_text, app_strong_text};
use ui::theme;
use wallet_ops::{
    DesktopSelfBroadcastResult, PublicBroadcasterCostEstimate, PublicBroadcasterResultKind,
    PublicBroadcasterSubmissionResult, SelfBroadcastAttemptInfo, SelfBroadcastCommand,
    SelfBroadcastCommandKind, SelfBroadcastCommandSender, SelfBroadcastGasFeeSelection,
    SponsoredSelfBroadcastCommand, SponsoredSelfBroadcastSessionOutcome,
    TransactionGenerationStage, self_broadcast_replacement_bumped_fee,
};

use super::gas_fee::{GasRetryInputs, format_gwei};
use super::private_action::{delivery_element_id, private_action_title_row};
use super::public_action::{
    ProgressDialogCloseBehavior, ProgressFooterAction, PublicActionStepStatus,
    progress_dialog_close_behavior, progress_footer_action, public_action_step_color,
    render_public_action_step_marker,
};
use super::public_broadcaster_cost::{
    PrivateBroadcasterProgressContext, PublicBroadcasterCostDisplay, cost_estimate_detail_text,
    private_broadcaster_context_row, private_broadcaster_context_row_with_action,
    render_private_broadcaster_progress_context, render_public_broadcaster_tx_hash_row,
};
use super::spend_authorization::spend_authorization_recipient_display;
use super::{
    DeliveryFormKind, PRIVATE_BROADCASTER_PROGRESS_DIALOG_WIDTH, UnshieldAssetKey, WalletRoot,
    app_panel, app_status_tag, app_step_row, app_stepper_container, dialog_max_height,
    format_native_token_amount_for_display, format_recipient_amount_with_native_top_up,
    secondary_dialog_content_width,
};

const SPONSORED_SHUTDOWN_ABORT_GRACE: Duration = Duration::from_secs(20);
const SPONSORED_CANCEL_ABORT_GRACE: Duration = Duration::from_secs(90);

use crate::assets::{RailgunActionIcon, WalletIconSource};

mod progress;
mod types;

#[cfg(test)]
pub(super) use progress::private_broadcaster_closed_active_stage;
pub(super) use progress::{
    apply_private_broadcaster_progress_stage, ensure_self_broadcast_unshield_progress_stage,
    fail_private_broadcaster_progress_steps_at_stage,
    finish_private_broadcaster_progress_steps_at_stage,
    finish_private_self_broadcast_progress_steps_at_stage,
    mark_private_broadcaster_active_step_stopped, private_broadcaster_closed_active_progress,
    private_broadcaster_progress_footer_action, private_broadcaster_progress_is_successful,
    private_broadcaster_progress_is_terminal, private_broadcaster_progress_steps,
    private_progress_stage_disables_stop, private_submission_discard_attempt_available,
    self_broadcast_progress_steps, self_broadcast_step_retry_kind,
    sponsored_stop_uses_session_command,
};
use progress::{
    ensure_private_broadcaster_progress_stage, private_broadcaster_progress_stop_available,
    private_broadcaster_retry_button_id, private_broadcaster_stage_detail,
    private_broadcaster_stage_id, private_broadcaster_stage_label, private_broadcaster_step_detail,
    public_broadcaster_waiting_can_stop,
};
#[cfg(test)]
pub(super) use progress::{
    finish_private_broadcaster_progress_steps, format_public_broadcaster_wait_remaining,
    public_broadcaster_wait_status_detail,
};
pub(super) use types::{
    PrivateBroadcasterClosedActiveProgress, PrivateBroadcasterProgressState,
    PrivateBroadcasterProgressStepState, PrivateSubmissionProgressFlow, SelfBroadcastGasRetryKind,
};
use types::{SELF_BROADCAST_GAS_RETRY_DIALOG_WIDTH, private_submission_dialog_title_action};

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

impl WalletRoot {
    pub(super) fn clear_private_broadcaster_progress_state(&mut self) {
        let sponsored_handle =
            cancel_private_broadcaster_progress(&mut self.private_broadcaster_progress);
        if let Some(sponsored_handle) = sponsored_handle {
            self.public_broadcaster_task_abort_handles
                .retain(|handle| handle.id() != sponsored_handle.id());
            spawn_sponsored_abort_watchdog(
                &self.runtime,
                sponsored_handle,
                SPONSORED_SHUTDOWN_ABORT_GRACE,
            );
        }
        cancel_public_broadcaster_tasks(&mut self.public_broadcaster_task_abort_handles);
        self.drop_trezor_pin_matrix_prompt();
    }

    pub(super) fn set_private_broadcaster_task_abort_handle(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        handle: tokio::task::AbortHandle,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if progress.kind == kind && progress.key == key && progress.generation_id == generation_id {
            self.public_broadcaster_task_abort_handles
                .retain(|handle| !handle.is_finished());
            self.public_broadcaster_task_abort_handles
                .push(handle.clone());
            progress.task_abort_handle = Some(handle);
        }
    }

    pub(super) fn start_private_broadcaster_progress(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        asset_label: String,
        icon_path: Option<WalletIconSource>,
        recipient: String,
        estimate: Option<PublicBroadcasterCostEstimate>,
        response_timeout: Duration,
        republish_interval: Duration,
    ) {
        let asset_label = Arc::<str>::from(asset_label);
        let dialog_open = self
            .private_broadcaster_progress
            .as_ref()
            .is_some_and(|progress| progress.dialog_open);
        self.private_broadcaster_progress = Some(PrivateBroadcasterProgressState {
            flow: PrivateSubmissionProgressFlow::PublicBroadcaster,
            kind,
            key,
            generation_id,
            asset_label: Arc::clone(&asset_label),
            icon_path,
            recipient: Arc::from(recipient),
            recipient_output: None,
            gas_payer: None,
            requires_device_approval: false,
            sponsored_funding: false,
            steps: private_broadcaster_progress_steps(),
            estimate,
            result: None,
            self_broadcast_result: None,
            self_broadcast_command_tx: None,
            sponsored_self_broadcast_command_tx: None,
            sponsored_self_broadcast_outcome: None,
            self_broadcast_attempts: Vec::new(),
            self_broadcast_current_gas_fee: None,
            self_broadcast_action_error: None,
            public_broadcaster_response_timeout: Some(response_timeout),
            public_broadcaster_republish_interval: Some(republish_interval),
            public_broadcaster_wait_started_at: None,
            task_abort_handle: None,
            stop_available: true,
            stopped: false,
            error: None,
            dialog_open,
            stage_seen: false,
        });
    }

    pub(super) fn start_private_self_broadcast_progress(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        asset_label: String,
        icon_path: Option<WalletIconSource>,
        recipient: String,
        recipient_output: Option<String>,
        gas_payer: String,
        requires_device_approval: bool,
        command_tx: Option<SelfBroadcastCommandSender>,
        current_gas_fee: Option<(u128, u128)>,
    ) {
        let asset_label = Arc::<str>::from(asset_label);
        let dialog_open = self
            .private_broadcaster_progress
            .as_ref()
            .is_some_and(|progress| progress.dialog_open);
        self.private_broadcaster_progress = Some(PrivateBroadcasterProgressState {
            flow: PrivateSubmissionProgressFlow::SelfBroadcast,
            kind,
            key,
            generation_id,
            asset_label: Arc::clone(&asset_label),
            icon_path,
            recipient: Arc::from(recipient),
            recipient_output: recipient_output.map(Arc::from),
            gas_payer: Some(Arc::from(gas_payer)),
            requires_device_approval,
            sponsored_funding: false,
            steps: self_broadcast_progress_steps(kind),
            estimate: None,
            result: None,
            self_broadcast_result: None,
            self_broadcast_command_tx: command_tx,
            sponsored_self_broadcast_command_tx: None,
            sponsored_self_broadcast_outcome: None,
            self_broadcast_attempts: Vec::new(),
            self_broadcast_current_gas_fee: current_gas_fee,
            self_broadcast_action_error: None,
            public_broadcaster_response_timeout: None,
            public_broadcaster_republish_interval: None,
            public_broadcaster_wait_started_at: None,
            task_abort_handle: None,
            stop_available: true,
            stopped: false,
            error: None,
            dialog_open,
            stage_seen: false,
        });
    }

    pub(super) fn show_private_broadcaster_progress_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if progress.dialog_open {
            return;
        }
        progress.dialog_open = true;
        let flow = progress.flow;
        let kind = progress.kind;
        let key = progress.key;
        let generation_id = progress.generation_id;
        let asset_label = Arc::clone(&progress.asset_label);
        let icon_path = progress.icon_path.clone();
        let root = cx.entity();
        let viewport_size = window.viewport_size();
        let dialog_width =
            (viewport_size.width * 0.92).min(PRIVATE_BROADCASTER_PROGRESS_DIALOG_WIDTH);
        let dialog_max_height = viewport_size.height * 0.84;
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(private_action_title_row(
                    private_submission_dialog_title_action(flow, kind),
                    asset_label.as_ref(),
                    icon_path.clone(),
                    None,
                    false,
                ))
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        let matches_progress = root
                            .private_broadcaster_progress
                            .as_ref()
                            .is_some_and(|progress| {
                                progress.kind == kind
                                    && progress.key == key
                                    && progress.generation_id == generation_id
                            });
                        if !matches_progress {
                            return;
                        }
                        root.apply_private_broadcaster_progress_dialog_close(window, cx, false);
                        cx.notify();
                    });
                })
                .child(
                    content_root
                        .read(cx)
                        .render_private_broadcaster_progress_dialog_content(
                            &content_root,
                            content_width,
                        ),
                )
        });
    }

    fn stop_private_broadcaster_progress(&mut self, cx: &mut Context<'_, Self>) {
        let (kind, key, generation_id, sponsored_stopping, sponsored_watchdog_handle) = {
            let Some(progress) = self.private_broadcaster_progress.as_mut() else {
                return;
            };
            if private_broadcaster_progress_footer_action(progress) != ProgressFooterAction::Stop {
                return;
            }
            let kind = progress.kind;
            let key = progress.key;
            let generation_id = progress.generation_id;
            let sponsored_session_active = progress.steps.iter().any(|step| {
                step.status == PublicActionStepStatus::Pending
                    && sponsored_stop_uses_session_command(step.stage)
            });
            let sponsored_stopping = if sponsored_session_active {
                request_sponsored_session_cancel(progress)
            } else {
                progress.sponsored_self_broadcast_command_tx.take();
                if let Some(handle) = progress.task_abort_handle.take() {
                    handle.abort();
                }
                false
            };
            progress.self_broadcast_command_tx = None;
            progress.self_broadcast_action_error = None;
            progress.stop_available = false;
            progress.error = None;
            if !sponsored_stopping {
                progress.stopped = true;
                mark_private_broadcaster_active_step_stopped(&mut progress.steps);
            }
            let sponsored_watchdog_handle = sponsored_stopping
                .then(|| progress.task_abort_handle.clone())
                .flatten();
            (
                kind,
                key,
                generation_id,
                sponsored_stopping,
                sponsored_watchdog_handle,
            )
        };
        self.clear_trezor_pin_matrix_prompt(cx);

        if sponsored_stopping {
            if let Some(handle) = sponsored_watchdog_handle {
                spawn_sponsored_abort_watchdog(&self.runtime, handle, SPONSORED_CANCEL_ABORT_GRACE);
            }
            cx.notify();
            return;
        }

        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key)
                    && form.generation_id == generation_id
                {
                    form.generating = false;
                    form.error = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key)
                    && form.generation_id == generation_id
                {
                    form.generating = false;
                    form.error = None;
                }
            }
        }
        cx.notify();
    }

    fn discard_private_submission_attempt(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        let matches_progress = self
            .private_broadcaster_progress
            .as_ref()
            .is_some_and(|progress| {
                progress.kind == kind
                    && progress.key == key
                    && progress.generation_id == generation_id
                    && progress
                        .steps
                        .iter()
                        .any(|step| matches!(step.status, PublicActionStepStatus::Error))
            });
        if !matches_progress {
            return;
        }
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key)
                    && form.generation_id == generation_id
                {
                    form.generating = false;
                    form.error = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key)
                    && form.generation_id == generation_id
                {
                    form.generating = false;
                    form.error = None;
                }
            }
        }
        self.clear_private_broadcaster_progress_state();
        cx.notify();
    }

    fn close_private_broadcaster_progress_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.apply_private_broadcaster_progress_dialog_close(window, cx, true);
        cx.notify();
    }

    fn apply_private_broadcaster_progress_dialog_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
        close_top_dialog: bool,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_ref() else {
            if close_top_dialog {
                window.close_dialog(cx);
            }
            return;
        };
        let kind = progress.kind;
        let key = progress.key;
        let behavior = progress_dialog_close_behavior(
            private_broadcaster_progress_is_successful(progress),
            progress.stopped,
        );
        match behavior {
            ProgressDialogCloseBehavior::AllAndClear => {
                match kind {
                    DeliveryFormKind::Send => self.close_send_form(key, cx),
                    DeliveryFormKind::Unshield => self.close_unshield_form(key, cx),
                }
                window.close_all_dialogs(cx);
            }
            ProgressDialogCloseBehavior::TopAndClear => {
                self.clear_private_broadcaster_progress_state();
                if close_top_dialog {
                    window.close_dialog(cx);
                }
            }
            ProgressDialogCloseBehavior::TopOnly => {
                self.clear_trezor_pin_matrix_prompt(cx);
                if let Some(progress) = self.private_broadcaster_progress.as_mut() {
                    progress.dialog_open = false;
                }
                if close_top_dialog {
                    window.close_dialog(cx);
                }
            }
        }
    }

    pub(super) fn update_private_broadcaster_progress_stage(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        stage: TransactionGenerationStage,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return false;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return false;
        }
        let should_open_dialog = !progress.stage_seen && !progress.dialog_open;
        progress.stage_seen = true;
        if progress.flow == PrivateSubmissionProgressFlow::PublicBroadcaster
            && stage == TransactionGenerationStage::WaitingForBroadcasterResponse
            && progress.public_broadcaster_wait_started_at.is_none()
        {
            progress.public_broadcaster_wait_started_at = Some(Instant::now());
        }
        if private_progress_stage_disables_stop(progress.flow, progress.sponsored_funding, stage) {
            progress.stop_available = false;
        }
        ensure_private_broadcaster_progress_stage(progress, stage);
        apply_private_broadcaster_progress_stage(&mut progress.steps, stage);
        cx.notify();
        should_open_dialog
    }

    pub(super) fn set_private_self_broadcast_unshield_poi_step(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        required: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id)
            || progress.flow != PrivateSubmissionProgressFlow::SelfBroadcast
            || progress.kind != DeliveryFormKind::Unshield
        {
            return;
        }
        if required {
            ensure_self_broadcast_unshield_progress_stage(
                &mut progress.steps,
                TransactionGenerationStage::GeneratingPoiProofs,
            );
        } else if let Some(index) = progress.steps.iter().position(|step| {
            step.stage == TransactionGenerationStage::GeneratingPoiProofs
                && step.status == PublicActionStepStatus::NotStarted
        }) {
            progress.steps.remove(index);
        }
        cx.notify();
    }

    pub(super) fn finish_private_broadcaster_progress(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        final_stage: TransactionGenerationStage,
        result: PublicBroadcasterSubmissionResult,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        finish_private_broadcaster_progress_steps_at_stage(
            &mut progress.steps,
            final_stage,
            &result.result,
        );
        progress.task_abort_handle = None;
        progress.stop_available = false;
        progress.result = Some(result);
        progress.error = None;
        cx.notify();
    }

    pub(super) fn finish_private_self_broadcast_progress(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        final_stage: TransactionGenerationStage,
        result: DesktopSelfBroadcastResult,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        ensure_private_broadcaster_progress_stage(progress, final_stage);
        finish_private_self_broadcast_progress_steps_at_stage(
            &mut progress.steps,
            final_stage,
            result.tx.status,
        );
        progress
            .self_broadcast_attempts
            .clone_from(&result.attempts);
        progress.self_broadcast_current_gas_fee =
            Some((result.max_fee_per_gas, result.max_priority_fee_per_gas));
        progress.self_broadcast_action_error = None;
        progress.self_broadcast_result = Some(result);
        progress.self_broadcast_command_tx = None;
        progress.task_abort_handle = None;
        progress.stop_available = false;
        progress.error = None;
        cx.notify();
    }

    pub(super) fn finish_private_sponsored_self_broadcast_progress(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        final_stage: TransactionGenerationStage,
        outcome: SponsoredSelfBroadcastSessionOutcome,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        progress.sponsored_self_broadcast_outcome = Some(outcome.clone());
        match outcome {
            SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(receipt) => {
                ensure_private_broadcaster_progress_stage(progress, final_stage);
                finish_private_self_broadcast_progress_steps_at_stage(
                    &mut progress.steps,
                    final_stage,
                    receipt.status,
                );
            }
            SponsoredSelfBroadcastSessionOutcome::Stopped { .. } => {
                mark_private_broadcaster_active_step_stopped(&mut progress.steps);
                progress.stopped = true;
            }
        }
        progress.sponsored_self_broadcast_command_tx = None;
        progress.task_abort_handle = None;
        progress.stop_available = false;
        progress.error = None;
        cx.notify();
    }

    pub(super) fn fail_private_broadcaster_progress(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        final_stage: TransactionGenerationStage,
        message: String,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        ensure_private_broadcaster_progress_stage(progress, final_stage);
        fail_private_broadcaster_progress_steps_at_stage(
            &mut progress.steps,
            final_stage,
            message.as_str(),
        );
        progress.self_broadcast_action_error = None;
        progress.error = Some(Arc::from(message));
        progress.self_broadcast_command_tx = None;
        progress.task_abort_handle = None;
        progress.stop_available = false;
        cx.notify();
    }

    pub(super) fn record_private_broadcaster_progress_step_error(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        stage: TransactionGenerationStage,
        message: &str,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return false;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return false;
        }
        progress.stage_seen = true;
        ensure_private_broadcaster_progress_stage(progress, stage);
        fail_private_broadcaster_progress_steps_at_stage(&mut progress.steps, stage, message);
        progress.self_broadcast_action_error = None;
        cx.notify();
        !progress.dialog_open
    }

    pub(super) fn record_private_self_broadcast_attempt(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        attempt: SelfBroadcastAttemptInfo,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        progress.self_broadcast_current_gas_fee =
            Some((attempt.max_fee_per_gas, attempt.max_priority_fee_per_gas));
        progress.self_broadcast_action_error = None;
        progress.stop_available = false;
        progress.self_broadcast_attempts.push(attempt);
        cx.notify();
    }

    pub(super) fn record_private_self_broadcast_attempt_rejected(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        message: String,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        progress.self_broadcast_action_error = Some(Arc::from(message));
        cx.notify();
    }

    pub(super) fn open_self_broadcast_gas_retry_dialog(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        retry_kind: SelfBroadcastGasRetryKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_ref() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id)
            || progress.self_broadcast_command_tx.is_none()
        {
            return;
        }
        let Some((mut max_fee, mut max_tip)) = progress.self_broadcast_current_gas_fee else {
            return;
        };
        if retry_kind == SelfBroadcastGasRetryKind::SpeedUp {
            max_fee = self_broadcast_replacement_bumped_fee(max_fee);
            max_tip = self_broadcast_replacement_bumped_fee(max_tip);
        }
        let root = cx.entity();
        let content = cx.new(|cx| {
            SelfBroadcastGasRetryDialogContent::new(
                root,
                kind,
                key,
                generation_id,
                retry_kind,
                max_fee,
                max_tip,
                window,
                cx,
            )
        });
        let dialog_width =
            (window.viewport_size().width * 0.92).min(SELF_BROADCAST_GAS_RETRY_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let submit_content = content.clone();
            dialog
                .on_ok(move |_event, window, cx| {
                    submit_content.update(cx, |content, cx| content.submit(window, cx));
                    false
                })
                .w(dialog_width)
                .max_h(dialog_max_height)
                .child(content.clone())
        });
    }

    fn submit_self_broadcast_gas_retry(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        retry_kind: SelfBroadcastGasRetryKind,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_ref() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        let Some(command_tx) = progress.self_broadcast_command_tx.as_ref() else {
            return;
        };
        let command_kind = match retry_kind {
            SelfBroadcastGasRetryKind::RetryStep | SelfBroadcastGasRetryKind::RetryEstimate => {
                SelfBroadcastCommandKind::Retry
            }
            SelfBroadcastGasRetryKind::SpeedUp => SelfBroadcastCommandKind::Replacement,
        };
        let send_result = command_tx.send(SelfBroadcastCommand {
            kind: command_kind,
            gas_fee: SelfBroadcastGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            },
        });
        if let Some(progress) = self.private_broadcaster_progress.as_mut() {
            progress.self_broadcast_action_error = send_result.err().map(|_| {
                Arc::from("Self-broadcast session is no longer accepting retry commands.")
            });
        }
        cx.notify();
    }

    fn submit_self_broadcast_step_retry(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_ref() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        let Some(command_tx) = progress.self_broadcast_command_tx.as_ref() else {
            return;
        };
        let gas_fee = progress.self_broadcast_current_gas_fee.map_or(
            SelfBroadcastGasFeeSelection::Auto,
            |(max_fee_per_gas, max_priority_fee_per_gas)| SelfBroadcastGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            },
        );
        let send_result = command_tx.send(SelfBroadcastCommand {
            kind: SelfBroadcastCommandKind::Retry,
            gas_fee,
        });
        if let Some(progress) = self.private_broadcaster_progress.as_mut() {
            progress.self_broadcast_action_error = send_result.err().map(|_| {
                Arc::from("Self-broadcast session is no longer accepting retry commands.")
            });
        }
        cx.notify();
    }

    pub(super) fn render_private_broadcaster_progress_dialog_content(
        &self,
        root: &Entity<Self>,
        content_width: Pixels,
    ) -> gpui::Div {
        let Some(progress) = self.private_broadcaster_progress.as_ref() else {
            return div()
                .w(content_width)
                .child(app_muted_text("No active private submission."));
        };
        let mut content = div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .child(render_private_broadcaster_progress_stepper(root, progress));

        match progress.flow {
            PrivateSubmissionProgressFlow::PublicBroadcaster => {
                if let Some(context) = self.private_broadcaster_progress_context(progress) {
                    let broadcaster_action =
                        self.render_public_broadcaster_preference_action(root, progress);
                    content = content.child(render_private_broadcaster_progress_context(
                        progress,
                        &context,
                        broadcaster_action,
                    ));
                } else {
                    content =
                        content.child(render_pending_public_broadcaster_progress_context(progress));
                }
            }
            PrivateSubmissionProgressFlow::SelfBroadcast => {
                content = content.child(render_self_broadcast_progress_context(progress));
            }
        }

        if let Some(result) = progress.result.as_ref()
            && let PublicBroadcasterResultKind::Submitted { tx_hash } = &result.result
        {
            content = content.child(render_public_broadcaster_tx_hash_row(
                tx_hash.clone(),
                delivery_element_id(progress.key, progress.kind, "progress-copy-public-tx"),
            ));
        }
        if let Some(result) = progress.self_broadcast_result.as_ref() {
            content = content.child(render_public_broadcaster_tx_hash_row(
                result.tx.tx_hash.clone(),
                delivery_element_id(progress.key, progress.kind, "progress-copy-self-tx"),
            ));
        }
        #[cfg(feature = "hardware")]
        if let Some(prompt) = self
            .hardware_profile_unlock
            .trezor_pin_matrix_prompt
            .as_ref()
        {
            content = content.child(super::vault_ui::render_trezor_pin_matrix_prompt(
                root, prompt,
            ));
        }
        content = content.child(render_private_broadcaster_progress_footer(
            root.clone(),
            progress,
        ));
        content
    }

    fn private_broadcaster_progress_context<'a>(
        &'a self,
        progress: &'a PrivateBroadcasterProgressState,
    ) -> Option<PrivateBroadcasterProgressContext<'a>> {
        if let Some(result) = progress.result.as_ref() {
            let anchor_rate = self
                .public_broadcaster_anchor_cache
                .cached_rate(result.broadcaster.chain_id, result.fee_token);
            return Some(PrivateBroadcasterProgressContext {
                display: PublicBroadcasterCostDisplay::from_result(
                    result,
                    anchor_rate,
                    Some(&self.effective_token_registry),
                ),
                anchor_cache: &self.public_broadcaster_anchor_cache,
            });
        }
        let estimate = progress.estimate.as_ref()?;
        let anchor_rate = self
            .public_broadcaster_anchor_cache
            .cached_rate(progress.key.chain_id, estimate.fee_token);
        Some(PrivateBroadcasterProgressContext {
            display: PublicBroadcasterCostDisplay::from_estimate_chain(
                progress.key.chain_id,
                estimate,
                anchor_rate,
                Some(&self.effective_token_registry),
            ),
            anchor_cache: &self.public_broadcaster_anchor_cache,
        })
    }

    fn render_public_broadcaster_preference_action(
        &self,
        root: &Entity<Self>,
        progress: &PrivateBroadcasterProgressState,
    ) -> Option<gpui::AnyElement> {
        let address = public_broadcaster_progress_address(progress)?;
        if self.is_banned_broadcaster(address) {
            return Some(
                render_broadcaster_preference_progress_chip("Banned", theme::DANGER)
                    .into_any_element(),
            );
        }

        let submitted = progress.result.as_ref().is_some_and(|result| {
            matches!(
                &result.result,
                PublicBroadcasterResultKind::Submitted { .. }
            )
        });
        if submitted {
            if self.is_favorite_broadcaster(address) {
                return Some(
                    render_broadcaster_preference_progress_chip("Favorited", theme::WARNING)
                        .into_any_element(),
                );
            }
            let action_root = root.clone();
            let address = address.to_owned();
            return Some(
                app_button_base(delivery_element_id(
                    progress.key,
                    progress.kind,
                    "favorite-current-broadcaster",
                ))
                .outline()
                .xsmall()
                .icon(Icon::new(IconName::Star))
                .accessibility_label("Add broadcaster to favorites")
                .tooltip(
                    "Save this broadcaster to your favorites so future transactions can prefer it.",
                )
                .on_click(move |_event, _window, cx| {
                    let address = address.clone();
                    action_root.update(cx, |root, cx| {
                        root.add_favorite_broadcaster(&address, cx);
                    });
                })
                .into_any_element(),
            );
        }

        if public_broadcaster_waiting_can_stop(progress, Instant::now()) && !progress.stop_available
        {
            let action_root = root.clone();
            let address = address.to_owned();
            return Some(
                app_button(
                    delivery_element_id(progress.key, progress.kind, "ban-current-broadcaster"),
                    "Ban this broadcaster",
                )
                .danger()
                .outline()
                .xsmall()
                .tooltip("Exclude this broadcaster from future selections. This does not stop the current wait.")
                .on_click(move |_event, _window, cx| {
                    let address = address.clone();
                    action_root.update(cx, |root, cx| {
                        root.add_banned_broadcaster(&address, cx);
                    });
                })
                .into_any_element(),
            );
        }

        None
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

fn render_broadcaster_preference_progress_chip(
    label: &'static str,
    color: u32,
) -> impl IntoElement {
    app_status_tag(label, color)
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

fn render_self_broadcast_progress_context(progress: &PrivateBroadcasterProgressState) -> gpui::Div {
    let recipient = progress.recipient.to_string();
    let recipient_copy_action = private_broadcaster_copy_action(
        format!(
            "private-self-broadcast-recipient-copy-{}",
            progress.generation_id
        ),
        recipient.clone(),
        "Copy recipient",
    );
    let mut context = div()
        .flex()
        .flex_col()
        .gap_2()
        .p(px(12.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(app_strong_text("Transaction context"))
        .child(private_broadcaster_context_row(
            if progress.sponsored_funding {
                "Transaction signer"
            } else {
                "Gas payer"
            },
            progress
                .gas_payer
                .as_deref()
                .unwrap_or("Selected Public account")
                .to_string(),
        ))
        .child(private_broadcaster_context_row_with_action(
            "Recipient",
            spend_authorization_recipient_display(&recipient),
            Some(recipient_copy_action),
        ));
    if let Some(result) = progress.self_broadcast_result.as_ref() {
        for (label, value) in self_broadcast_composite_output_rows(progress, result) {
            context = context.child(private_broadcaster_context_row(label, value));
        }
        context = context
            .child(private_broadcaster_context_row(
                "Max fee",
                format!("{} gwei", format_gwei(result.max_fee_per_gas)),
            ))
            .child(private_broadcaster_context_row(
                "Max tip",
                format!("{} gwei", format_gwei(result.max_priority_fee_per_gas)),
            ))
            .child(private_broadcaster_context_row(
                "Estimated gas cost",
                format_native_token_amount_for_display(
                    result.chain_id,
                    result.estimated_native_gas_cost,
                ),
            ))
            .child(private_broadcaster_context_row(
                "Receipt",
                if result.tx.status {
                    "confirmed"
                } else {
                    "reverted"
                }
                .to_string(),
            ))
            .child(private_broadcaster_context_row(
                "Block",
                result.tx.block_number.to_string(),
            ));
    }
    if let Some(SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(receipt)) =
        progress.sponsored_self_broadcast_outcome.as_ref()
    {
        context = context
            .child(private_broadcaster_context_row(
                "Receipt",
                if receipt.status {
                    "confirmed"
                } else {
                    "reverted"
                }
                .to_owned(),
            ))
            .child(private_broadcaster_context_row_with_action(
                "Transaction hash",
                receipt.tx_hash.clone(),
                Some(private_broadcaster_copy_action(
                    format!(
                        "private-self-broadcast-tx-hash-copy-{}",
                        progress.generation_id
                    ),
                    receipt.tx_hash.clone(),
                    "Copy transaction hash",
                )),
            ))
            .child(private_broadcaster_context_row(
                "Block",
                receipt.block_number.to_string(),
            ));
    }
    context
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
    let now = Instant::now();
    let action = progress_footer_action(
        private_broadcaster_progress_stop_available(progress, now),
        private_broadcaster_progress_is_terminal(progress),
    );
    let button_root = root;
    let (id_suffix, label) = match action {
        ProgressFooterAction::Stop => (
            "progress-stop",
            if public_broadcaster_waiting_can_stop(progress, now) && !progress.stop_available {
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

pub(super) fn render_private_broadcaster_progress_stepper(
    root: &Entity<WalletRoot>,
    progress: &PrivateBroadcasterProgressState,
) -> gpui::Div {
    let steps = &progress.steps;
    let mut stepper = app_stepper_container();
    let last_index = steps.len().saturating_sub(1);
    for (index, step) in steps.iter().enumerate() {
        stepper = stepper.child(render_private_broadcaster_progress_step(
            root.clone(),
            progress,
            step,
            index == last_index,
        ));
    }
    stepper
}

fn render_private_broadcaster_progress_step(
    root: Entity<WalletRoot>,
    progress: &PrivateBroadcasterProgressState,
    step: &PrivateBroadcasterProgressStepState,
    is_last: bool,
) -> gpui::Div {
    let color = public_action_step_color(step.status);
    let mut body = div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .pb(if is_last { px(0.0) } else { px(12.0) })
        .child(
            app_strong_text(private_broadcaster_stage_label(
                step.stage,
                progress.requires_device_approval,
            ))
            .text_color(rgb(color))
            .line_height(gpui::relative(1.0)),
        );
    if step.status == PublicActionStepStatus::Error {
        let message = step
            .message
            .as_deref()
            .unwrap_or("This broadcaster submission step failed.");
        let copy_id = SharedString::from(format!(
            "wallet-private-broadcaster-{}-error-copy",
            private_broadcaster_stage_id(step.stage),
        ));
        body = body.child(
            div()
                .flex()
                .items_start()
                .gap_1()
                .child(
                    app_muted_text(message.to_string())
                        .flex_1()
                        .min_w(px(0.0))
                        .whitespace_normal()
                        .text_color(rgb(theme::DANGER))
                        .line_height(gpui::relative(1.0)),
                )
                .child(clipboard_with_toast(copy_id, message.to_string())),
        );
    } else {
        let detail = private_broadcaster_step_detail(progress, step, Instant::now());
        body = body.child(
            app_muted_text(detail)
                .text_color(rgb(color))
                .line_height(gpui::relative(1.0)),
        );
    }
    if let Some(action) = render_self_broadcast_step_action(root, progress, step) {
        body = body.child(action);
    }

    app_step_row(
        render_public_action_step_marker(step.status, color),
        body,
        is_last,
        color,
        px(32.0),
        None,
    )
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
            "Open the broadcaster status dialog for the timeout details.",
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
    let (title, detail, border) = if result.tx.status {
        (
            "Self-broadcast confirmed",
            "Open the self-broadcast status dialog for transaction details.",
            theme::SUCCESS,
        )
    } else {
        (
            "Self-broadcast reverted",
            "Open the self-broadcast status dialog for receipt details.",
            theme::DANGER,
        )
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
