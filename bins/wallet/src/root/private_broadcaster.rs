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

mod cancellation;
mod gas_retry_dialog;
mod preference_control;
mod progress_context;
mod progress_steps;
mod status_notices;

pub(super) use cancellation::*;
pub(super) use preference_control::*;
pub(super) use progress_context::*;
pub(super) use progress_steps::*;
pub(super) use status_notices::*;

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

/// What a submission's progress shows of its broadcaster's preference state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum BroadcasterPreferenceControl {
    Banned,
    Favorited,
    AddFavorite,
    Ban,
}
