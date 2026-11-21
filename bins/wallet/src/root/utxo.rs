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

mod display_rows;
mod ppoi_labels;
mod refund_guidance;
mod row_cells;
mod table_delegate;

pub(super) use display_rows::*;
pub(super) use ppoi_labels::*;
use refund_guidance::{
    blocked_shield_refund_available_footer, blocked_shield_refund_close_button,
    blocked_shield_refund_guidance, blocked_shield_refund_origin_row,
    blocked_shield_refund_unavailable_footer, blocked_shield_rescue_error_info,
    blocked_shield_rescue_in_flight_info, blocked_shield_rescue_submitted_info,
};
pub(super) use row_cells::*;
pub(super) use table_delegate::*;

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
        let finality_unavailable = block == 0 || depth == 0 || safe_head > head || head < block;
        if finality_unavailable {
            return None;
        }
        if safe_head >= block {
            return Some(BlockFinalityProgress::Safe);
        }
        let elapsed = head - block;
        (elapsed < depth).then_some(BlockFinalityProgress::Confirming { elapsed, depth })
    }
}
