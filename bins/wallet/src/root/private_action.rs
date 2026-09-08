use std::sync::Arc;
use std::time::Duration;

use crate::assets::{RailgunActionIcon, WalletIconSource};
use alloy::primitives::{Address, U256};
use broadcaster_monitor::FeeRow;
use gpui::{
    Animation, AnimationExt as _, App, AppContext, Bounds, Context, ElementId, Entity, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, Pixels, RenderOnce,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Window, anchored, canvas,
    deferred, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, IndexPath, Selectable, Sizable, WindowExt,
    alert::Alert,
    button::{Button, ButtonGroup, ButtonVariants},
    collapsible::Collapsible,
    input::{Escape as InputEscape, Input, InputEvent, InputState, Position},
    popover::Popover,
    scroll::ScrollableElement,
    select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState},
    spinner::Spinner,
    tooltip::Tooltip,
};
use railgun_ui::{format_token_amount, short_address};
use rand::seq::IndexedRandom;
use tokio::sync::{mpsc, watch};
use ui::controls::{
    FullWidthSelectItems, app_button, app_button_base, app_button_label, app_inline_control_row,
    app_input, app_muted_text, app_segment_button, app_strong_text,
};
use ui::theme::{self, APP_FONT_FAMILY, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::{
    BroadcasterFeePolicy, DesktopNativeTopUpPlan, DesktopNativeTopUpRequest,
    DesktopPrivateSpendAuthorization, DesktopSelfBroadcastCostEstimate, DesktopSelfBroadcastResult,
    DesktopSendCalldataRequest, DesktopSendPublicBroadcasterRequest,
    DesktopSendSelfBroadcastRequest, DesktopSponsoredSelfBroadcastResult,
    DesktopSponsoredSendSelfBroadcastRequest, DesktopSponsoredUnshieldSelfBroadcastRequest,
    DesktopUnshieldCalldataRequest, DesktopUnshieldPublicBroadcasterRequest,
    DesktopUnshieldSelfBroadcastRequest, FeeHandlingMode, ListUtxosOutput, PreparedSendCall,
    PreparedUnshieldCall, PublicAssetId, PublicBalanceAmount, PublicBalanceEntry,
    PublicBalanceSnapshot, PublicBroadcasterCandidate, PublicBroadcasterCostEstimate,
    PublicBroadcasterResultKind, PublicBroadcasterSubmissionResult, RAILGUN_PROTOCOL_FEE_BPS,
    SelfBroadcastGasFeeQuote, SelfBroadcastGasFeeSelection, SelfBroadcastSessionEvent,
    SponsoredAuthorizationLimit, SponsoredIncentive, SponsoredSelfBroadcastCommand,
    SponsoredSelfBroadcastSessionOutcome, SponsorshipError, SponsorshipPayment,
    TokenAnchorRateCache, TransactionGenerationStage, WalletSession,
    estimate_desktop_send_self_broadcast_cost, estimate_desktop_unshield_self_broadcast_cost,
    expected_eip1559_fee_per_gas, fee_policy_eligible_public_broadcasters,
    native_top_up_policy_for_chain, native_top_up_primary_recipient_amount_for_fee_mode,
    native_top_up_required_wrapped_native_amount_for_fee_mode, native_top_up_wrapped_native_amount,
    parse_railgun_recipient, parse_send_amount, parse_unshield_amount,
    prepare_desktop_send_calldata, prepare_desktop_unshield_calldata,
    public_action_maximum_gas_cost_is_significant, quote_desktop_self_broadcast_gas_fee,
    quote_sponsored_send_authorization_limit, quote_sponsored_unshield_authorization_limit,
    select_public_broadcaster_with_policy_and_trust,
    settings::EffectiveTokenRegistry,
    sponsorship_payment, submit_desktop_send_public_broadcaster,
    submit_desktop_send_self_broadcast, submit_desktop_sponsored_send_self_broadcast,
    submit_desktop_sponsored_unshield_self_broadcast, submit_desktop_unshield_public_broadcaster,
    submit_desktop_unshield_self_broadcast, unshield_protocol_fee_amount_for_fee_mode,
    vault::{
        DesktopVaultStore, DesktopViewSession, PrivateAddressBookEntry, PublicAccountMetadata,
        PublicAccountSource, PublicAccountStatus, PublicAddressBookEntry, WalletMetadataBundle,
        WalletStatus,
    },
};

use super::broadcaster_picker::{
    BroadcasterChoice, broadcaster_choice_supported_by_candidates,
    selected_broadcaster_fee_warning, selected_broadcaster_label,
    should_preserve_estimate_after_broadcaster_policy_change,
};
use super::gas_fee::{
    Eip1559GasFeeEditTarget, Eip1559GasFeeEditorState, Eip1559GasFeeMode, Eip1559GasFeeTarget,
    format_gwei, render_eip1559_gas_fee_editor,
};
use super::private_assets::{
    build_send_asset, build_unshield_asset, format_private_asset_rows_from_snapshot,
    max_unshield_amount_from_snapshot, refresh_form_asset_from_snapshot,
};
use super::private_broadcaster::{
    private_broadcaster_closed_active_progress, render_private_broadcaster_status_notice,
    render_private_self_broadcast_status_notice, render_private_submission_active_status_notice,
};
use super::public_account::public_account_display_label;
use super::public_action::{
    PublicActionFeeDisplay, public_action_protocol_fee_label, render_public_action_fee_estimate,
};
use super::public_balances::public_balance_entry_for_chain;
use super::public_broadcaster::resolve_selected_public_broadcaster_fee_token;
use super::public_broadcaster_cost::{
    cost_estimate_detail_text, public_broadcaster_cost_status,
    render_public_broadcaster_cost_estimate, render_public_broadcaster_cost_status,
    should_render_public_broadcaster_cost_preview,
};
use super::spend_authorization::{
    SpendAuthorizationIntent, SpendAuthorizationSummary, SpendAuthorizationSummaryRow,
    is_spend_authorization_failure_error,
};
use super::utxo::short_hash;
use super::{
    ChainUtxoState, PRIVATE_ASSET_LIST_WIDTH, PublicBroadcasterFeeTokenOption, WalletRoot,
    copyable_mono_field, dialog_max_height, effective_fee_handling_mode,
    format_exact_token_amount_for_display, format_native_token_amount_ceiling_for_display,
    format_native_token_amount_for_display, format_native_top_up_recipient_suffix,
    format_recipient_amount_with_native_top_up, format_report_chain, format_send_amount_input,
    format_token_amount_ceiling_for_display, format_token_amount_for_display,
    format_unshield_amount_input, format_value_with_usd_label, is_effective_wrapped_native_token,
    labeled_field, native_token_display_label, native_wrapped_output_labels, new_prefilled_input,
    new_text_input, parse_address, public_balance_amount_label,
    public_broadcaster_fee_token_warning, public_broadcaster_submit_disabled_for_fee_token_options,
    secondary_dialog_content_width, send_form_max_entered_amount, should_show_fee_mode_toggle,
    token_display_metadata, token_label_row, unshield_form_max_entered_amount,
    unshield_max_entered_amount_for_mode, vault_error_kind,
};

mod delivery;
mod form_lifecycle;
mod generation;
mod helpers;
mod recipient_picker;
mod render_forms;
mod render_helpers;
mod self_broadcast;
mod types;

pub(super) use delivery::*;
#[cfg(test)]
pub(super) use generation::sponsored_authorization_display;
pub(super) use helpers::*;
pub(super) use recipient_picker::*;
pub(super) use render_helpers::*;
pub(super) use self_broadcast::*;
pub(super) use types::*;
