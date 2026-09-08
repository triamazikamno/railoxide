#[cfg(feature = "hardware")]
use super::{
    Address, Arc, Context, DEFAULT_HARDWARE_DERIVATION_PATH, DesktopVaultStore, DesktopViewSession,
    Duration, Focusable, HardwareDerivationDescriptor, HardwareDerivationError,
    HardwareDerivationMethod, HardwareDeviceKind, HardwareProfileBindingKind,
    HardwareProfileMetadata, HardwareProfileSession, HardwareRailgunAccountMetadata,
    HardwareSetupErrorFocus, HardwareViewAccessKey, HardwareWalletProfile,
    HardwareWalletSyncIntent, LedgerHardwareDerivationClient, ParentElement, Styled,
    SyntheticRailgunEntropy, TrezorHardwareDerivationClient, TrezorPassphraseMode,
    TrezorPinMatrixProvider, TrezorPinMatrixRequestKind, VaultError, VaultState, ViewUnlock,
    WalletMetadataBundle, WalletRoot, WalletSetupMode, Window, WindowExt, Zeroize, Zeroizing,
    app_strong_text, generate_opaque_id, hardware_device_kind_from_source,
    hardware_setup_vault_error_focus, hardware_setup_vault_error_message,
    hardware_setup_vault_error_preserves_password, hardware_view_access_key_from_hardware_output,
    mpsc, parse_bip32_path, px, secondary_dialog_content_width, sleep,
    synthetic_entropy_from_hardware_output, trezor_cipher_key_label, vault_error_kind,
    vault_error_message, wallet_options_from_metadata,
};
#[cfg(not(feature = "hardware"))]
use super::{Context, HardwareWalletSyncIntent, WalletRoot};

pub(super) mod creation;
pub(super) mod dialog;
mod labels;
pub(super) mod parsing;
pub(super) mod picker;
pub(super) mod progress;
pub(super) mod types;

#[cfg(any(feature = "hardware", test))]
pub(in crate::root) use creation::hardware_wallet_creation_result_is_current;
#[cfg(feature = "hardware")]
use creation::{create_hardware_profile_accounts, open_hardware_account, unlock_hardware_profile};
pub(in crate::root) use labels::hardware_device_label;
pub(in crate::root) use parsing::default_hardware_wallet_setup_intent;
#[cfg(any(feature = "hardware", test))]
pub(in crate::root) use parsing::{
    HARDWARE_PROFILE_ADD_SUBACCOUNT_BUTTON_ID, HARDWARE_PROFILE_RECOVER_EXACT_BUTTON_ID,
    HARDWARE_PROFILE_RECOVER_RANGE_BUTTON_ID, effective_trezor_passphrase_mode,
    hardware_profile_label_warning, hardware_session_needs_trezor_app_passphrase,
    next_trezor_passphrase_mode, parse_hardware_exact_recovery_index,
    parse_hardware_recovery_range, parse_hardware_wallet_restore_account_index,
    trezor_passphrase_mode_copy,
};
#[cfg(feature = "hardware")]
pub(in crate::root) use parsing::{
    hardware_profile_evm_address_for_session, trezor_session_stale_error_message,
};
#[cfg(feature = "hardware")]
use picker::{default_hardware_profile_label, hardware_profile_approval_prompt_for_descriptor};
#[cfg(feature = "hardware")]
pub(in crate::root) use picker::{
    hardware_account_picker_rows, hardware_profile_approval_prompt_for_account,
    hardware_profile_auto_open_wallet_id,
};
#[cfg(all(test, feature = "hardware"))]
pub(in crate::root) use progress::hardware_profile_detection_ledger_is_unlocked;
#[cfg(feature = "hardware")]
use progress::{
    HARDWARE_PROFILE_READINESS_RETRY_INTERVAL, send_hardware_profile_approval_progress,
    send_hardware_profile_progress, send_hardware_profile_readiness_progress,
    send_trezor_passphrase_policy_progress, trezor_pin_matrix_provider,
};
#[cfg(feature = "hardware")]
pub(in crate::root) use progress::{
    hardware_profile_detection_should_retry,
    hardware_profile_detection_should_suppress_initial_ledger_progress,
    hardware_profile_detection_should_suppress_initial_trezor_progress,
    hardware_profile_hardware_error_message, hardware_profile_should_reconnect_after_error,
    hardware_setup_error_preserves_password,
};
#[cfg(feature = "hardware")]
pub(in crate::root) use types::{
    HardwareAccountPickerRow, HardwareProfileApprovalPrompt, HardwareProfilePickerView,
    HardwareProfileStep, HardwareProfileStepState, HardwareProfileStepStatus,
    HardwareProfileUnlockPurpose, HardwareProfileUnlockState, TrezorPinMatrixPromptState,
    dismiss_hardware_profile_unlock_state,
};
#[cfg(feature = "hardware")]
use types::{
    HardwareProfilePinMatrixRequest, HardwareProfileProgressUpdate, HardwareWalletCreationError,
    HardwareWalletCreationResult, default_hardware_profile_steps, hardware_approval_error,
};
