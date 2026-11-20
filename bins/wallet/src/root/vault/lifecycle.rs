use std::cell::Cell;
use std::collections::BTreeSet;
use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use super::super::chain_load::{
    WalletSyncLifecycleCleanupReport, WalletSyncLifecycleCleanupWaitGroup,
};
#[cfg(not(feature = "hardware"))]
use super::hardware_device_wallet_select_label;
use super::{
    Arc, BroadcasterActivityTab, ChainUtxoState, Context, DesktopViewSession, ParentElement,
    PendingSoftwareProfileOpen, RememberedWalletKind, SearchableVec, Styled, VaultError,
    VaultState, ViewUnlock, WalletMetadataBundle, WalletOption, WalletRoot, WalletSetupMode,
    Window, WindowExt, Zeroizing, app_strong_text, default_wallet_label_for_metadata,
    dialog_max_height, hardware_device_kind_from_wallet_select_value, px,
    secondary_dialog_content_width, vault_error_kind, vault_error_message, visible_wallet_metadata,
    wallet_options_from_metadata, wallet_select_items_from_metadata,
    wallet_select_value_for_selected_wallet,
};
use ui::dialog::WalletDialogExt as _;

const PENDING_SOFTWARE_PROFILE_OPEN_TIMEOUT: Duration = Duration::from_mins(5);
const WALLET_REPLACEMENT_DELAYED_THRESHOLD: Duration = Duration::from_secs(10);
const WALLET_REPLACEMENT_PRESENTATION_CUTOFF: Duration = Duration::from_secs(30);
pub(in crate::root) const WALLET_REPLACEMENT_TIMEOUT_MESSAGE: &str =
    "Your wallet is still closing. Try again in a moment.";

pub(in crate::root) enum WalletReplacementCleanupWaitOutcome {
    Completed(Result<WalletSyncLifecycleCleanupReport, String>),
    PresentationTimedOut,
}

pub(in crate::root) async fn wait_for_wallet_replacement_cleanup<F>(
    cleanup: WalletSyncLifecycleCleanupWaitGroup,
    presentation_cutoff: F,
) -> WalletReplacementCleanupWaitOutcome
where
    F: Future<Output = ()>,
{
    tokio::select! {
        cleanup_result = cleanup.shutdown_for_wallet_replacement() => {
            WalletReplacementCleanupWaitOutcome::Completed(cleanup_result)
        }
        () = presentation_cutoff => WalletReplacementCleanupWaitOutcome::PresentationTimedOut,
    }
}

pub(super) const fn pending_software_profile_open_timeout_is_current(
    captured_lifetime_generation: u64,
    current_lifetime_generation: u64,
    is_pending: bool,
) -> bool {
    // The timeout belongs to the pending lifetime, even while an operation owns its payload.
    is_pending && captured_lifetime_generation == current_lifetime_generation
}

struct WalletContextInstallation {
    remote_unlock: Option<Arc<wallet_ops::gateway::GatewayUnlockAttempt>>,
    gateway_unlock: Option<u64>,
    session: Arc<DesktopViewSession>,
    metadata: Vec<WalletMetadataBundle>,
    created_wallet_init_policy: wallet_ops::CreatedWalletChainInitPolicy,
    protected_seed_session: Option<wallet_ops::vault::ProtectedSoftwareSeedSession>,
    active_wallet_generation: u64,
    wallet_switch_generation: u64,
    profile_open_operation_generation: u64,
    profile_open_lifetime_generation: u64,
    selected_chain: u64,
    #[cfg(feature = "hardware")]
    active_hardware_profile: Option<HardwareProfileMetadata>,
    cleanup: WalletSyncLifecycleCleanupWaitGroup,
}

pub(in crate::root) const fn wallet_replacement_finalize_is_admitted(
    captured_wallet_generation: u64,
    current_wallet_generation: u64,
    captured_chain: u64,
    current_chain: u64,
    deleting_wallet: bool,
    switching_wallet: bool,
) -> bool {
    !deleting_wallet
        && captured_wallet_generation == current_wallet_generation
        && captured_chain == current_chain
        && switching_wallet
}

pub(super) fn pending_profile_result_is_current(
    captured_operation_generation: u64,
    current_operation_generation: u64,
    captured_wallet_generation: u64,
    current_wallet_generation: u64,
    captured_base_profile_uuid: &str,
    current_base_profile_uuid: Option<&str>,
    selected_wallet_id: Option<&str>,
    captured_chain: u64,
    current_chain: u64,
    is_pending: bool,
) -> bool {
    is_pending
        && captured_operation_generation == current_operation_generation
        && captured_wallet_generation == current_wallet_generation
        && current_base_profile_uuid == Some(captured_base_profile_uuid)
        && selected_wallet_id.is_none()
        && captured_chain == current_chain
}

pub(super) fn clear_wallet_context_visibility<T>(
    view_session: &mut Option<Arc<T>>,
    selected_wallet_id: &mut Option<Arc<str>>,
    revealed_passphrase_context_id: &mut Option<Arc<str>>,
    wallet_metadata: &mut Vec<WalletMetadataBundle>,
    wallet_options: &mut Vec<WalletOption>,
    public_accounts: &mut Vec<wallet_ops::vault::PublicAccountMetadata>,
) {
    *view_session = None;
    *selected_wallet_id = None;
    *revealed_passphrase_context_id = None;
    wallet_metadata.clear();
    wallet_options.clear();
    public_accounts.clear();
}

pub(in crate::root) const fn vault_lock_is_allowed(
    maintenance_idle: bool,
    wallet_deletion_in_progress: bool,
) -> bool {
    maintenance_idle && !wallet_deletion_in_progress
}
#[cfg(feature = "hardware")]
use super::{
    HardwareProfileMetadata, HardwareProfileUnlockPurpose, HardwareProfileUnlockState,
    hardware_device_kind_from_source,
};

mod pending_profile_open;
mod unlocked_entry;
mod vault_locking;
mod view_session_install;
mod wallet_switch;

/// Keeps the tracing target of the moved methods at this module's path.
const LOG_TARGET: &str = module_path!();

pub(in crate::root) const fn wallet_replacement_update_is_current(
    captured_wallet_generation: u64,
    current_wallet_generation: u64,
    captured_switch_generation: u64,
    current_switch_generation: u64,
    captured_operation_generation: u64,
    current_operation_generation: u64,
    captured_lifetime_generation: u64,
    current_lifetime_generation: u64,
    captured_chain: u64,
    current_chain: u64,
    deleting_wallet: bool,
    switching_wallet: bool,
) -> bool {
    wallet_replacement_finalize_is_admitted(
        captured_wallet_generation,
        current_wallet_generation,
        captured_chain,
        current_chain,
        deleting_wallet,
        switching_wallet,
    ) && captured_switch_generation == current_switch_generation
        && captured_operation_generation == current_operation_generation
        && captured_lifetime_generation == current_lifetime_generation
}

#[cfg(test)]
mod tests;
