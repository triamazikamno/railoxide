use std::sync::Arc;
#[cfg(feature = "hardware")]
use std::time::Duration;

#[cfg(feature = "hardware")]
use alloy::primitives::Address;
use gpui::{Context, Entity, Focusable, ParentElement, Styled, Window, px};
use gpui_component::{WindowExt, input::InputState, select::SearchableVec};
#[cfg(feature = "hardware")]
use tokio::{sync::mpsc, time::sleep};
use ui::controls::app_strong_text;
#[cfg(feature = "hardware")]
use wallet_ops::hardware::{
    DEFAULT_HARDWARE_DERIVATION_PATH, HardwareDerivationDescriptor, HardwareDerivationError,
    HardwareDerivationMethod, HardwareViewAccessKey, SyntheticRailgunEntropy,
    hardware_view_access_key_from_hardware_output,
    ledger::LedgerHardwareDerivationClient,
    parse_bip32_path, synthetic_entropy_from_hardware_output,
    trezor::{
        TrezorHardwareDerivationClient, TrezorPinMatrixProvider, TrezorPinMatrixRequestKind,
        trezor_cipher_key_label,
    },
};
use wallet_ops::hardware::{HardwareDeviceKind, HardwareWalletSyncIntent};
use wallet_ops::settings::RememberedWalletKind;
#[cfg(feature = "hardware")]
use wallet_ops::vault::{
    DesktopVaultStore, HardwareProfileBindingKind, HardwareProfileSession, HardwareWalletProfile,
    TrezorPassphraseMode,
};
use wallet_ops::vault::{
    DesktopViewSession, HardwareProfileMetadata, HardwareRailgunAccountMetadata,
    PRIMARY_WALLET_LABEL, SpendUnlock, VaultError, VaultSessionId, ViewUnlock,
    WalletMetadataBundle, WalletSoftwareContextKind, WalletSource,
    default_wallet_label_for_metadata, generate_opaque_id, generate_seed_material,
    sort_wallet_metadata,
};
#[cfg(feature = "hardware")]
use zeroize::Zeroize;
use zeroize::Zeroizing;

use super::wallet_header::WalletSelectItem;
use super::{
    BroadcasterActivityTab, ChainUtxoState, WalletRoot, WalletTab, dialog_max_height,
    secondary_dialog_content_width,
};

mod change_password;
mod errors;
mod hardware;
mod inputs;
mod lifecycle;
mod passphrase_ui;
mod pending;
#[cfg(test)]
pub(in crate::root) use lifecycle::vault_lock_is_allowed;
mod selection;
mod setup;
mod types;

#[cfg(any(feature = "hardware", test))]
pub(in crate::root) use errors::hardware_setup_vault_error_message;
#[cfg(feature = "hardware")]
pub(in crate::root::vault) use errors::{
    HardwareSetupErrorFocus, hardware_setup_vault_error_focus,
    hardware_setup_vault_error_preserves_password,
};
pub(in crate::root) use errors::{vault_error_kind, vault_error_message};
#[cfg(all(test, feature = "hardware"))]
pub(in crate::root) use hardware::hardware_profile_detection_ledger_is_unlocked;
#[cfg(any(feature = "hardware", test))]
#[allow(unused_imports)]
pub(in crate::root) use hardware::{
    HARDWARE_PROFILE_ADD_SUBACCOUNT_BUTTON_ID, HARDWARE_PROFILE_RECOVER_EXACT_BUTTON_ID,
    HARDWARE_PROFILE_RECOVER_RANGE_BUTTON_ID, effective_trezor_passphrase_mode,
    hardware_profile_label_warning, hardware_session_needs_trezor_app_passphrase,
    hardware_wallet_creation_result_is_current, next_trezor_passphrase_mode,
    parse_hardware_exact_recovery_index, parse_hardware_recovery_range,
    parse_hardware_wallet_restore_account_index, trezor_passphrase_mode_copy,
};
#[cfg(feature = "hardware")]
#[allow(unused_imports)]
pub(in crate::root) use hardware::{
    HardwareAccountPickerRow, HardwareProfileApprovalPrompt, HardwareProfilePickerView,
    HardwareProfileStep, HardwareProfileStepState, HardwareProfileStepStatus,
    HardwareProfileUnlockPurpose, HardwareProfileUnlockState, TrezorPinMatrixPromptState,
    dismiss_hardware_profile_unlock_state, hardware_account_picker_rows,
    hardware_profile_approval_prompt_for_account, hardware_profile_auto_open_wallet_id,
    hardware_profile_detection_should_retry,
    hardware_profile_detection_should_suppress_initial_ledger_progress,
    hardware_profile_detection_should_suppress_initial_trezor_progress,
    hardware_profile_evm_address_for_session, hardware_profile_hardware_error_message,
    hardware_profile_should_reconnect_after_error, hardware_setup_error_preserves_password,
    trezor_session_stale_error_message,
};
pub(in crate::root) use hardware::{default_hardware_wallet_setup_intent, hardware_device_label};
#[cfg(test)]
pub(in crate::root) use inputs::should_focus_vault_input;
#[cfg(test)]
pub(in crate::root) use lifecycle::{
    WALLET_REPLACEMENT_TIMEOUT_MESSAGE, WalletReplacementCleanupWaitOutcome,
    wait_for_wallet_replacement_cleanup, wallet_replacement_finalize_is_admitted,
    wallet_replacement_update_is_current,
};
pub(in crate::root) use passphrase_ui::PassphraseOpenUi;
pub(in crate::root) use pending::{PendingSoftwareProfileOpen, PendingSoftwareProfileOpenStage};
#[cfg(feature = "hardware")]
pub(in crate::root::vault) use selection::hardware_device_kind_from_source;
#[allow(unused_imports)]
pub(in crate::root) use selection::{
    HardwareWalletDisplayInfo, hardware_device_kind_from_wallet_select_value,
    hardware_device_wallet_select_label, hardware_wallet_display_info,
    passphrase_open_action_is_eligible, remembered_wallet_id_for_restore, visible_wallet_metadata,
    wallet_options_from_metadata, wallet_select_items_from_metadata,
    wallet_select_value_for_selected_wallet,
};
#[cfg(test)]
pub(in crate::root) use setup::{SoftwareProfileRestoreAction, software_profile_restore_action};
pub(in crate::root) use types::{VaultState, WalletOption, WalletSetupMode};

#[allow(dead_code)]
pub(in crate::root) const fn hardware_device_wallet_select_value(
    device_kind: HardwareDeviceKind,
) -> &'static str {
    selection::hardware_device_wallet_select_value(device_kind)
}
