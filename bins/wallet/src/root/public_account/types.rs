use std::collections::BTreeSet;
use std::sync::Arc;

use alloy::primitives::Address;
use gpui::Entity;
use gpui_component::input::InputState;
use wallet_ops::{
    PublicActionAttemptInfo, PublicActionCommandSender, PublicActionGasFeeSelection,
    PublicActionProgressStep, PublicAdvancedTransactionEstimate, PublicAssetId,
};

use crate::assets::WalletIconSource;
use crate::root::gas_fee::Eip1559GasFeeEditorState;
use crate::root::public_action::{
    PublicActionFeeAuthorizationReview, PublicActionMode, PublicActionProgressLifecycle,
    PublicActionStepState, PublicSendKind,
};

use super::hardware::HardwarePublicAccountDerivationStatus;

pub(in crate::root) struct PublicAccountFormState {
    pub(in crate::root) add_label_input: Entity<InputState>,
    pub(in crate::root) add_password_input: Entity<InputState>,
    pub(in crate::root) import_label_input: Entity<InputState>,
    pub(in crate::root) import_private_key_input: Entity<InputState>,
    pub(in crate::root) import_password_input: Entity<InputState>,
    pub(in crate::root) edit_label_input: Entity<InputState>,
    pub(in crate::root) search_input: Entity<InputState>,
    pub(in crate::root) send_recipient_input: Entity<InputState>,
    pub(in crate::root) send_amount_input: Entity<InputState>,
    pub(in crate::root) advanced_send_to_input: Entity<InputState>,
    pub(in crate::root) advanced_send_value_input: Entity<InputState>,
    pub(in crate::root) advanced_send_data_input: Entity<InputState>,
    pub(in crate::root) shield_amount_input: Entity<InputState>,
    pub(in crate::root) send_gas_fee: Eip1559GasFeeEditorState,
    pub(in crate::root) shield_gas_fee: Eip1559GasFeeEditorState,
    pub(in crate::root) shield_gas_fee_authorization_ceiling: Option<PublicActionGasFeeSelection>,
    pub(in crate::root) import_global: bool,
    pub(in crate::root) selected_account_uuid: Option<Arc<str>>,
    pub(in crate::root) editing_account_uuid: Option<Arc<str>>,
    pub(in crate::root) search_query: Arc<str>,
    pub(in crate::root) selected_asset: Option<PublicAssetId>,
    pub(in crate::root) mimic_railway_shield: bool,
    pub(in crate::root) action_mode: PublicActionMode,
    pub(in crate::root) action_progress_mode: PublicActionMode,
    pub(in crate::root) public_send_kind: PublicSendKind,
    pub(in crate::root) advanced_send_estimate: Option<PublicAdvancedTransactionEstimate>,
    pub(in crate::root) advanced_send_estimate_invalidated: bool,
    pub(in crate::root) advanced_send_estimate_pending: bool,
    pub(in crate::root) advanced_send_estimate_generation: u64,
    pub(in crate::root) advanced_send_to_error: Option<Arc<str>>,
    pub(in crate::root) advanced_send_value_error: Option<Arc<str>>,
    pub(in crate::root) advanced_send_data_error: Option<Arc<str>>,
    pub(in crate::root) action_generation: u64,
    pub(in crate::root) action_progress: Vec<PublicActionStepState>,
    pub(in crate::root) action_progress_lifecycle: PublicActionProgressLifecycle,
    pub(in crate::root) action_fee_authorization_review: Option<PublicActionFeeAuthorizationReview>,
    pub(in crate::root) expanded_action_error_steps: BTreeSet<PublicActionProgressStep>,
    pub(in crate::root) action_progress_dialog_open: bool,
    pub(in crate::root) action_requires_device_approval: bool,
    pub(in crate::root) action_progress_title_override: Option<Arc<str>>,
    pub(in crate::root) action_progress_asset_label: Arc<str>,
    pub(in crate::root) action_progress_icon_path: Option<WalletIconSource>,
    pub(in crate::root) action_task_abort_handle: Option<tokio::task::AbortHandle>,
    pub(in crate::root) action_stop_available: bool,
    pub(in crate::root) action_stopped: bool,
    pub(in crate::root) action_command_tx: Option<PublicActionCommandSender>,
    pub(in crate::root) action_attempts: Vec<PublicActionAttemptInfo>,
    pub(in crate::root) action_current_gas_fee: Option<(u128, u128)>,
    pub(in crate::root) action_fees_authorized: bool,
    pub(in crate::root) action_action_error: Option<Arc<str>>,
    pub(in crate::root) action_contract_address: Option<Arc<str>>,
    pub(in crate::root) next_derived_index: Option<u32>,
    pub(in crate::root) next_account_label_number: u32,
    pub(in crate::root) error: Option<Arc<str>>,
    pub(in crate::root) send_error: Option<Arc<str>>,
    pub(in crate::root) shield_error: Option<Arc<str>>,
    pub(in crate::root) adding_account: bool,
    pub(in crate::root) hardware_derivation_status: HardwarePublicAccountDerivationStatus,
    pub(in crate::root) hardware_confirmation_address: Option<Address>,
    pub(in crate::root) importing_account: bool,
    pub(in crate::root) sending: bool,
    pub(in crate::root) shielding: bool,
    pub(in crate::root) active_accounts_open: bool,
    pub(in crate::root) inactive_accounts_open: bool,
    pub(in crate::root) pending_global_delete_uuid: Option<Arc<str>>,
}
