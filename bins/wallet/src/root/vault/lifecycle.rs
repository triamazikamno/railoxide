use std::collections::BTreeSet;
use std::future::Future;
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
    WalletTab, Window, WindowExt, Zeroizing, app_strong_text, default_wallet_label_for_metadata,
    dialog_content_max_height, dialog_max_height, hardware_device_kind_from_wallet_select_value,
    px, scrollable_dialog_content, secondary_dialog_content_width, vault_error_kind,
    vault_error_message, visible_wallet_metadata, wallet_options_from_metadata,
    wallet_select_items_from_metadata, wallet_select_value_for_selected_wallet,
};

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

impl WalletRoot {
    pub(in crate::root) fn select_wallet(
        &mut self,
        wallet_id: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.manage_wallets.deleting_wallet_id.is_some() {
            self.sync_wallet_select(window, cx);
            return;
        }
        if let Some(device_kind) = hardware_device_kind_from_wallet_select_value(wallet_id) {
            #[cfg(feature = "hardware")]
            {
                self.open_hardware_profile_unlock_dialog_for_device(
                    device_kind,
                    HardwareProfileUnlockPurpose::Open,
                    window,
                    cx,
                );
                self.sync_wallet_select(window, cx);
            }
            #[cfg(not(feature = "hardware"))]
            {
                self.set_vault_error(
                    format!(
                        "{} support is not enabled in this build.",
                        hardware_device_wallet_select_label(device_kind)
                    ),
                    cx,
                );
                self.sync_wallet_select(window, cx);
            }
            return;
        }
        if self.selected_wallet_id.as_deref() == Some(wallet_id) {
            return;
        }
        window.close_all_dialogs(cx);
        self.switch_active_wallet(wallet_id, window, cx);
    }

    pub(in crate::root) fn open_add_wallet_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_add_wallet_dialog_with_mode(WalletSetupMode::Choose, window, cx);
    }

    pub(super) fn open_add_wallet_dialog_with_mode(
        &mut self,
        initial_mode: WalletSetupMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.generated_seed = None;
        self.vault_error = None;
        self.wallet_setup_mode = initial_mode;
        let label = default_wallet_label_for_metadata(&self.wallet_metadata);
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(520.0));
        let dialog_max_height = dialog_max_height(window);
        let content_max_height = dialog_content_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Add wallet"))
                .child(scrollable_dialog_content(
                    content_max_height,
                    content_root
                        .read(cx)
                        .render_add_wallet_dialog_content(content_root.clone(), content_width),
                ))
        });
        cx.defer_in(window, move |root, window, cx| {
            root.set_wallet_name_input(&label, window, cx);
            root.add_wallet_password_input
                .update(cx, |input, cx| input.set_value("", window, cx));
        });
    }

    #[cfg_attr(not(feature = "hardware"), allow(clippy::needless_pass_by_ref_mut))]
    pub(super) fn switch_active_wallet(
        &mut self,
        wallet_id: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.manage_wallets.deleting_wallet_id.is_some() {
            self.sync_wallet_select(window, cx);
            return;
        }
        #[cfg(feature = "hardware")]
        if self.wallet_metadata.iter().any(|metadata| {
            metadata.wallet_uuid == wallet_id
                && hardware_device_kind_from_source(metadata.source).is_some()
        }) {
            self.open_hardware_profile_unlock_dialog_for_wallet(
                Arc::from(wallet_id.to_owned()),
                window,
                cx,
            );
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            self.set_vault_error("Wallet vault storage is unavailable", cx);
            return;
        };
        let Some(current_session) = self.view_session.clone() else {
            self.open_wallet_from_vault_view_unlock(wallet_id, window, cx);
            return;
        };

        let current_wallet_id: Arc<str> = Arc::from(current_session.wallet_id().to_owned());
        let active_wallet_generation = self.active_wallet_generation;
        self.wallet_switch_generation = self.wallet_switch_generation.wrapping_add(1);
        let switch_generation = self.wallet_switch_generation;
        self.vault_error = None;
        let wallet_id_string = wallet_id.to_owned();
        let metadata = self.wallet_metadata.clone();
        let join = self.runtime.spawn_blocking(move || {
            store.load_view_session_with_view_session(current_session.as_ref(), &wallet_id_string)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.wallet_switch_generation != switch_generation
                    || !root.is_active_wallet_generation(
                        current_wallet_id.as_ref(),
                        active_wallet_generation,
                    )
                {
                    return;
                }
                match result {
                    Ok(Ok(session)) => root.install_view_session(session, &metadata, window, cx),
                    Ok(Err(error)) => {
                        root.handle_vault_error(&error, cx);
                        root.sync_wallet_select(window, cx);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "wallet view-session task failed");
                        root.set_vault_error(
                            "Failed to open wallet. See logs for non-sensitive diagnostics.",
                            cx,
                        );
                        root.sync_wallet_select(window, cx);
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn open_wallet_from_vault_view_unlock(
        &mut self,
        wallet_id: &str,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.clone() else {
            self.set_vault_error("Wallet vault storage is unavailable", cx);
            return;
        };
        let Some(vault_view_unlock) = self.vault_view_unlock.clone() else {
            self.set_vault_error("Wallet vault is locked", cx);
            return;
        };

        let active_wallet_generation = self.active_wallet_generation;
        self.wallet_switch_generation = self.wallet_switch_generation.wrapping_add(1);
        let switch_generation = self.wallet_switch_generation;
        self.vault_error = None;
        let wallet_id_string = wallet_id.to_owned();
        let metadata = self.wallet_metadata.clone();
        let join = self.runtime.spawn_blocking(move || {
            store.load_view_session_with_view_unlock(&vault_view_unlock, &wallet_id_string)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.wallet_switch_generation != switch_generation
                    || root.active_wallet_generation != active_wallet_generation
                {
                    return;
                }
                match result {
                    Ok(Ok(session)) => root.install_view_session(session, &metadata, window, cx),
                    Ok(Err(error)) => {
                        root.handle_vault_error(&error, cx);
                        root.sync_wallet_select(window, cx);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "wallet view-session task failed");
                        root.set_vault_error(
                            "Failed to open wallet. See logs for non-sensitive diagnostics.",
                            cx,
                        );
                        root.sync_wallet_select(window, cx);
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    #[allow(dead_code)]
    pub(super) fn deactivate_wallet_and_switch(
        &mut self,
        wallet_id: &str,
        password: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.clone() else {
            self.set_vault_error("Wallet vault storage is unavailable", cx);
            return;
        };
        if let Err(error) = store.deactivate_wallet(password, wallet_id) {
            self.handle_vault_error(&error, cx);
            return;
        }
        let metadata = match store.list_wallet_metadata(password) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.handle_vault_error(&error, cx);
                return;
            }
        };
        let visible_metadata =
            visible_wallet_metadata(&metadata, self.revealed_passphrase_context_id.as_deref());
        self.wallet_metadata.clone_from(&visible_metadata);
        self.wallet_options = wallet_options_from_metadata(visible_metadata);

        if self.selected_wallet_id.as_deref() != Some(wallet_id) {
            self.sync_wallet_select(window, cx);
            cx.notify();
            return;
        }

        let Some(next_wallet_id) = self
            .wallet_options
            .first()
            .map(|option| Arc::clone(&option.wallet_id))
        else {
            self.set_vault_error("No active wallet remains after deactivation", cx);
            return;
        };
        match store.load_view_session(password, next_wallet_id.as_ref()) {
            Ok(session) => self.install_view_session(session, &metadata, window, cx),
            Err(error) => self.handle_vault_error(&error, cx),
        }
    }

    pub(super) fn install_view_session(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.install_view_session_with_dialog_policy(
            session,
            metadata,
            true,
            wallet_ops::CreatedWalletChainInitPolicy::Resumed,
            window,
            cx,
        );
    }

    pub(in crate::root) fn install_view_session_after_management(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.install_view_session_with_dialog_policy(
            session,
            metadata,
            false,
            wallet_ops::CreatedWalletChainInitPolicy::Resumed,
            window,
            cx,
        );
    }

    pub(super) fn install_verified_software_context(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        protected_seed_session: Option<wallet_ops::vault::ProtectedSoftwareSeedSession>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.begin_view_session_installation(
            session,
            metadata,
            true,
            wallet_ops::CreatedWalletChainInitPolicy::Resumed,
            protected_seed_session,
            window,
            cx,
        );
    }

    #[cfg(feature = "hardware")]
    fn active_hardware_profile_for_wallet(
        &self,
        wallet_id: &str,
        metadata: &[WalletMetadataBundle],
    ) -> Option<HardwareProfileMetadata> {
        let account = metadata
            .iter()
            .find(|wallet| wallet.wallet_uuid == wallet_id)
            .and_then(|wallet| wallet.hardware_account.as_ref())?;

        self.hardware_profile_unlock
            .profile
            .as_ref()
            .filter(|profile| profile.profile_id == account.profile_id)
            .cloned()
            .or_else(|| {
                self.active_hardware_profile
                    .as_ref()
                    .filter(|profile| profile.profile_id == account.profile_id)
                    .cloned()
            })
    }

    fn install_view_session_with_dialog_policy(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        close_dialogs: bool,
        created_wallet_init_policy: wallet_ops::CreatedWalletChainInitPolicy,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.begin_view_session_installation(
            session,
            metadata,
            close_dialogs,
            created_wallet_init_policy,
            None,
            window,
            cx,
        );
    }

    fn begin_view_session_installation(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        close_dialogs: bool,
        created_wallet_init_policy: wallet_ops::CreatedWalletChainInitPolicy,
        protected_seed_session: Option<wallet_ops::vault::ProtectedSoftwareSeedSession>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.manage_wallets.deleting_wallet_id.is_some() {
            self.sync_wallet_select(window, cx);
            return;
        }
        #[cfg(feature = "hardware")]
        let active_hardware_profile =
            self.active_hardware_profile_for_wallet(session.wallet_id(), metadata);
        self.clear_protected_software_seed_session(cx);
        self.supersede_wallet_sessions();
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
        let profile_open_operation_generation =
            self.pending_software_profile_open_operation_generation;
        let profile_open_lifetime_generation =
            self.pending_software_profile_open_lifetime_generation;
        if close_dialogs {
            window.close_all_dialogs(cx);
        }
        self.advance_active_wallet_generation();
        self.wallet_switch_generation = self.wallet_switch_generation.wrapping_add(1);
        let wallet_switch_generation = self.wallet_switch_generation;
        clear_wallet_context_visibility(
            &mut self.view_session,
            &mut self.selected_wallet_id,
            &mut self.revealed_passphrase_context_id,
            &mut self.wallet_metadata,
            &mut self.wallet_options,
            &mut self.public_accounts,
        );
        self.vault_view_unlock = None;
        self.vault_state = VaultState::SwitchingWallet;
        self.wallet_switch_delayed = false;
        self.wallet_setup_mode = WalletSetupMode::Choose;
        self.focus_vault_input_on_render = false;
        self.setup_password = None;
        self.generated_seed = None;
        self.clear_key_export_dialog_state(window, cx);
        #[cfg(feature = "hardware")]
        {
            self.active_hardware_profile = None;
            self.hardware_profile_unlock = HardwareProfileUnlockState::default();
            self.clear_hardware_profile_sensitive_inputs(window, cx);
        }
        self.hardware_wallet_creation_in_progress = false;
        self.hardware_wallet_creation_generation =
            self.hardware_wallet_creation_generation.wrapping_add(1);
        self.hardware_wallet_creation_intent = None;
        self.clear_hardware_wallet_restore_account_index(window, cx);
        self.vault_error = None;
        self.reset_wallet_scoped_state(cx);
        self.private_address_book.clear();
        self.public_address_book.clear();
        self.sync_wallet_select(window, cx);
        cx.notify();

        let installation = WalletContextInstallation {
            session: Arc::new(session),
            metadata: metadata.to_vec(),
            created_wallet_init_policy,
            protected_seed_session,
            active_wallet_generation: self.active_wallet_generation,
            wallet_switch_generation,
            profile_open_operation_generation,
            profile_open_lifetime_generation,
            selected_chain: self.selected_chain,
            #[cfg(feature = "hardware")]
            active_hardware_profile,
            cleanup: self.wallet_sync_cleanup_wait_group(),
        };
        let cleanup = installation.cleanup.clone();
        let captured_wallet_generation = installation.active_wallet_generation;
        let captured_switch_generation = installation.wallet_switch_generation;
        let captured_operation_generation = installation.profile_open_operation_generation;
        let captured_lifetime_generation = installation.profile_open_lifetime_generation;
        let captured_chain = installation.selected_chain;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(WALLET_REPLACEMENT_DELAYED_THRESHOLD)
                .await;
            let _ = this.update_in(cx, |root, _window, cx| {
                if wallet_replacement_update_is_current(
                    captured_wallet_generation,
                    root.active_wallet_generation,
                    captured_switch_generation,
                    root.wallet_switch_generation,
                    captured_operation_generation,
                    root.pending_software_profile_open_operation_generation,
                    captured_lifetime_generation,
                    root.pending_software_profile_open_lifetime_generation,
                    captured_chain,
                    root.selected_chain,
                    root.manage_wallets.deleting_wallet_id.is_some(),
                    root.view_session.is_none()
                        && root.vault_view_unlock.is_none()
                        && matches!(root.vault_state, VaultState::SwitchingWallet),
                ) {
                    root.wallet_switch_delayed = true;
                    cx.notify();
                }
            });
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            match wait_for_wallet_replacement_cleanup(
                cleanup,
                cx.background_executor().timer(WALLET_REPLACEMENT_PRESENTATION_CUTOFF),
            )
            .await
            {
                WalletReplacementCleanupWaitOutcome::Completed(cleanup_result) => {
                    let _ = this.update_in(cx, |root, window, cx| {
                        let installation = installation;
                        if !wallet_replacement_update_is_current(
                            captured_wallet_generation,
                            root.active_wallet_generation,
                            captured_switch_generation,
                            root.wallet_switch_generation,
                            captured_operation_generation,
                            root.pending_software_profile_open_operation_generation,
                            captured_lifetime_generation,
                            root.pending_software_profile_open_lifetime_generation,
                            captured_chain,
                            root.selected_chain,
                            root.manage_wallets.deleting_wallet_id.is_some(),
                            root.view_session.is_none()
                                && root.vault_view_unlock.is_none()
                                && matches!(root.vault_state, VaultState::SwitchingWallet),
                        ) {
                            return;
                        }
                        let report = match cleanup_result {
                            Ok(report) => report,
                            Err(error) => {
                                tracing::warn!(%error, "wallet replacement sync cleanup failed");
                                root.set_wallet_replacement_error(
                                    "Previous wallet sync cleanup could not be completed. Retry the wallet replacement before continuing.",
                                    cx,
                                );
                                return;
                            }
                        };
                        if report.failed_startup_tasks != 0 {
                            root.set_wallet_replacement_error(
                                "Previous wallet sync cleanup failed. Retry the wallet replacement.",
                                cx,
                            );
                            return;
                        }
                        root.finish_view_session_installation(installation, window, cx);
                    });
                }
                WalletReplacementCleanupWaitOutcome::PresentationTimedOut => {
                    let _ = this.update_in(cx, |root, window, cx| {
                        if wallet_replacement_update_is_current(
                            captured_wallet_generation,
                            root.active_wallet_generation,
                            captured_switch_generation,
                            root.wallet_switch_generation,
                            captured_operation_generation,
                            root.pending_software_profile_open_operation_generation,
                            captured_lifetime_generation,
                            root.pending_software_profile_open_lifetime_generation,
                            captured_chain,
                            root.selected_chain,
                            root.manage_wallets.deleting_wallet_id.is_some(),
                            root.view_session.is_none()
                                && root.vault_view_unlock.is_none()
                                && matches!(root.vault_state, VaultState::SwitchingWallet),
                        ) {
                            root.abandon_wallet_replacement_installation(window, cx);
                        }
                    });
                    drop(installation);
                }
            }
        })
        .detach();
    }

    fn finish_view_session_installation(
        &mut self,
        installation: WalletContextInstallation,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let WalletContextInstallation {
            session,
            metadata,
            created_wallet_init_policy,
            protected_seed_session,
            active_wallet_generation: _,
            wallet_switch_generation: _,
            profile_open_operation_generation: _,
            profile_open_lifetime_generation: _,
            selected_chain: _,
            #[cfg(feature = "hardware")]
            active_hardware_profile,
            cleanup: _,
        } = installation;
        let wallet_id: Arc<str> = Arc::from(session.wallet_id().to_owned());
        let vault_view_unlock = Arc::new(session.clone_vault_view_unlock());
        self.invalidate_pending_profile_open_tokens();
        let visible_metadata = visible_wallet_metadata(&metadata, Some(wallet_id.as_ref()));
        self.revealed_passphrase_context_id = visible_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == wallet_id.as_ref())
            .and_then(|metadata| {
                metadata
                    .software_context
                    .as_ref()
                    .filter(|context| {
                        context.kind == wallet_ops::vault::WalletSoftwareContextKind::Passphrase
                    })
                    .map(|_| Arc::clone(&wallet_id))
            });
        self.view_session = Some(Arc::clone(&session));
        self.install_vault_view_unlock(vault_view_unlock);
        self.wallet_metadata = visible_metadata;
        self.wallet_options = wallet_options_from_metadata(self.wallet_metadata.clone());
        self.selected_wallet_id = Some(Arc::clone(&wallet_id));
        let selected_metadata = self
            .wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == wallet_id.as_ref());
        if selected_metadata.is_some_and(|metadata| metadata.source.is_hardware_derived())
            || session.hardware_profile_session().is_some()
        {
            self.ui_state.last_wallet_id = Some(wallet_id.as_ref().to_owned());
            self.ui_state.last_wallet_kind = RememberedWalletKind::HardwareWallet;
        } else {
            self.ui_state.last_wallet_id = Some(
                selected_metadata
                    .and_then(|metadata| metadata.software_context.as_ref())
                    .map_or_else(
                        || wallet_id.as_ref().to_owned(),
                        |context| context.base_profile_uuid.clone(),
                    ),
            );
            self.ui_state.last_wallet_kind = RememberedWalletKind::SoftwareProfile;
        }
        self.save_ui_state();
        #[cfg(feature = "hardware")]
        {
            self.active_hardware_profile = if selected_metadata
                .is_some_and(|metadata| metadata.source.is_hardware_derived())
                || session.hardware_profile_session().is_some()
            {
                active_hardware_profile
            } else {
                None
            };
        }
        self.sync_wallet_select(window, cx);
        self.reset_wallet_scoped_state(cx);
        self.protected_software_seed_session = protected_seed_session.map(Arc::new);
        self.reload_address_books(cx);
        self.reload_broadcaster_preferences(cx);
        self.reload_public_accounts(window, cx);
        if self.active_activity == super::super::sidebar::Activity::Proposals {
            match self.governance.tab {
                super::super::governance::GovernanceTab::Proposals => {
                    self.start_proposals_refresh(false, cx);
                }
                super::super::governance::GovernanceTab::Staking => {
                    self.start_staking_refresh(cx);
                }
            }
        }
        self.setup_password = None;
        self.generated_seed = None;
        #[cfg(feature = "hardware")]
        {
            self.hardware_profile_unlock = HardwareProfileUnlockState::default();
            self.clear_hardware_profile_sensitive_inputs(window, cx);
        }
        self.hardware_wallet_creation_in_progress = false;
        self.hardware_wallet_creation_generation =
            self.hardware_wallet_creation_generation.wrapping_add(1);
        self.hardware_wallet_creation_intent = None;
        self.clear_hardware_wallet_restore_account_index(window, cx);
        self.add_wallet_password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.import_mnemonic_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.clear_key_export_dialog_state(window, cx);
        self.vault_error = None;
        self.vault_state = VaultState::ViewUnlocked;
        self.wallet_switch_delayed = false;
        self.wallet_setup_mode = WalletSetupMode::Choose;
        self.ensure_waku_started(cx);
        self.ensure_chain_load_with_start_policy(
            self.selected_chain,
            Some(created_wallet_init_policy.sync_start_policy()),
            cx,
        );
        if created_wallet_init_policy != wallet_ops::CreatedWalletChainInitPolicy::Resumed {
            self.initialize_created_wallet_chain_metadata(created_wallet_init_policy);
        }
        cx.notify();
    }

    fn abandon_wallet_replacement_installation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.wallet_switch_generation = self.wallet_switch_generation.wrapping_add(1);
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
        self.clear_protected_software_seed_session(cx);
        clear_wallet_context_visibility(
            &mut self.view_session,
            &mut self.selected_wallet_id,
            &mut self.revealed_passphrase_context_id,
            &mut self.wallet_metadata,
            &mut self.wallet_options,
            &mut self.public_accounts,
        );
        self.vault_view_unlock = None;
        self.private_address_book.clear();
        self.public_address_book.clear();
        self.reset_wallet_scoped_state(cx);
        self.setup_password = None;
        self.generated_seed = None;
        self.clear_key_export_dialog_state(window, cx);
        #[cfg(feature = "hardware")]
        {
            self.active_hardware_profile = None;
            self.hardware_profile_unlock = HardwareProfileUnlockState::default();
            self.clear_hardware_profile_sensitive_inputs(window, cx);
        }
        self.wallet_switch_delayed = false;
        self.vault_error = Some(Arc::from(WALLET_REPLACEMENT_TIMEOUT_MESSAGE));
        self.vault_state = VaultState::UnlockVault;
        self.wallet_setup_mode = WalletSetupMode::Choose;
        self.focus_vault_input_on_render = true;
        self.sync_wallet_select(window, cx);
        cx.notify();
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) fn sync_wallet_select(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let items = wallet_select_items_from_metadata(&self.wallet_metadata);
        let selected_value = self.selected_wallet_id.as_ref().map(|wallet_id| {
            wallet_select_value_for_selected_wallet(wallet_id, &self.wallet_metadata)
        });
        self.wallet_select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(items), window, cx);
            if let Some(value) = selected_value.as_ref() {
                select.set_selected_value(value, window, cx);
            } else {
                select.set_selected_index(None, window, cx);
            }
        });
    }

    pub(super) fn enter_view_unlocked(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.install_view_session(session, metadata, window, cx);
    }

    pub(super) fn enter_new_wallet_view_unlocked(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.install_view_session_with_dialog_policy(
            session,
            metadata,
            true,
            wallet_ops::CreatedWalletChainInitPolicy::InitialCreate,
            window,
            cx,
        );
    }

    pub(super) fn enabled_chain_ids_for_created_wallet(&self) -> BTreeSet<u64> {
        self.effective_chain_configs
            .values()
            .filter(|chain| chain.enabled)
            .map(|chain| chain.chain_id)
            .collect()
    }

    fn initialize_created_wallet_chain_metadata(
        &mut self,
        init_policy: wallet_ops::CreatedWalletChainInitPolicy,
    ) {
        let Some(view_session) = self.view_session.clone() else {
            return;
        };
        let Some(vault_store) = self.vault_store.as_ref() else {
            return;
        };
        let effective_chains = self.effective_chain_configs.clone();
        let db = vault_store.db();
        let http = self.http.clone();
        let skip_chain_id = Some(self.selected_chain);

        let task = self.runtime.spawn(async move {
            wallet_ops::initialize_created_wallet_chain_metadata_for_session(
                view_session,
                effective_chains,
                db,
                http,
                skip_chain_id,
                init_policy,
            )
            .await;
        });
        self.wallet_sync_lifecycle.track_wallet_task(task);
    }

    pub(in crate::root) fn enter_password_metadata_unlocked(
        &mut self,
        metadata: &[WalletMetadataBundle],
        vault_view_unlock: Arc<ViewUnlock>,
        setup_password: Option<Zeroizing<String>>,
        pending_software_profile_open: Option<PendingSoftwareProfileOpen>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.clear_protected_software_seed_session(cx);
        let active = wallet_options_from_metadata(metadata.to_owned());
        if active.is_empty() {
            if let Some(password) = setup_password {
                self.set_default_wallet_name_from_password(password.as_str(), window, cx);
                self.setup_password = Some(password);
            }
            self.install_vault_view_unlock(vault_view_unlock);
            self.vault_error = None;
            self.vault_state = VaultState::SetupWallet;
            self.wallet_setup_mode = WalletSetupMode::Choose;
            self.ensure_waku_started(cx);
            cx.notify();
            return;
        }

        if let Some(pending) = pending_software_profile_open {
            self.enter_pending_software_profile_open(pending, window, cx);
            return;
        }

        window.close_all_dialogs(cx);
        self.advance_active_wallet_generation();
        self.pending_software_profile_base_profile_uuid = None;
        self.view_session = None;
        self.install_vault_view_unlock(vault_view_unlock);
        self.revealed_passphrase_context_id = None;
        self.wallet_metadata = visible_wallet_metadata(metadata, None);
        self.wallet_options = wallet_options_from_metadata(self.wallet_metadata.clone());
        self.selected_wallet_id = None;
        self.sync_wallet_select(window, cx);
        self.shutdown_wallet_session_store();
        self.reset_wallet_scoped_state(cx);
        self.setup_password = None;
        self.generated_seed = None;
        self.clear_key_export_dialog_state(window, cx);
        #[cfg(feature = "hardware")]
        {
            self.active_hardware_profile = None;
            self.hardware_profile_unlock = HardwareProfileUnlockState::default();
            self.clear_hardware_profile_sensitive_inputs(window, cx);
        }
        self.hardware_wallet_creation_in_progress = false;
        self.hardware_wallet_creation_generation =
            self.hardware_wallet_creation_generation.wrapping_add(1);
        self.hardware_wallet_creation_intent = None;
        self.clear_hardware_wallet_restore_account_index(window, cx);
        self.vault_error = None;
        self.vault_state = VaultState::ViewUnlocked;
        self.wallet_setup_mode = WalletSetupMode::Choose;
        self.ensure_waku_started(cx);
        cx.notify();
    }

    pub(super) fn enter_pending_software_profile_open(
        &mut self,
        mut pending: PendingSoftwareProfileOpen,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.pending_software_profile_open_lifetime_generation = self
            .pending_software_profile_open_lifetime_generation
            .wrapping_add(1);
        self.pending_software_profile_open_operation_generation = self
            .pending_software_profile_open_operation_generation
            .wrapping_add(1);
        pending.set_operation_generation(self.pending_software_profile_open_operation_generation);
        self.pending_software_profile_base_profile_uuid =
            Some(Arc::clone(&pending.base_profile_uuid));
        self.pending_software_profile_open = Some(pending);
        self.shutdown_wallet_session_store();
        self.advance_active_wallet_generation();
        self.clear_protected_software_seed_session(cx);
        clear_wallet_context_visibility(
            &mut self.view_session,
            &mut self.selected_wallet_id,
            &mut self.revealed_passphrase_context_id,
            &mut self.wallet_metadata,
            &mut self.wallet_options,
            &mut self.public_accounts,
        );
        self.vault_view_unlock = None;
        self.private_address_book.clear();
        self.public_address_book.clear();
        self.set_broadcaster_preferences(wallet_ops::vault::BroadcasterPreferences::default(), cx);
        self.broadcaster_preference_error = None;
        self.reset_wallet_scoped_state(cx);
        self.sync_wallet_select(window, cx);
        self.setup_password = None;
        self.generated_seed = None;
        self.vault_error = None;
        self.vault_state = VaultState::PendingSoftwareProfileOpen;
        self.wallet_setup_mode = WalletSetupMode::Choose;
        cx.notify();
        self.schedule_pending_software_profile_open_timeout(window, cx);
        let passphrase_open_ui = self.passphrase_open_ui.clone();
        cx.defer_in(window, move |_root, window, cx| {
            passphrase_open_ui.update(cx, |ui, cx| ui.focus_passphrase(window, cx));
        });
    }

    fn schedule_pending_software_profile_open_timeout(
        &self,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        let captured_lifetime_generation = self.pending_software_profile_open_lifetime_generation;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(PENDING_SOFTWARE_PROFILE_OPEN_TIMEOUT)
                .await;
            let _ = this.update_in(cx, |root, window, cx| {
                if pending_software_profile_open_timeout_is_current(
                    captured_lifetime_generation,
                    root.pending_software_profile_open_lifetime_generation,
                    matches!(root.vault_state, VaultState::PendingSoftwareProfileOpen),
                ) {
                    root.abandon_pending_software_profile_open(window, cx);
                }
            });
        })
        .detach();
    }

    pub(in crate::root) fn pending_software_profile_open_stage(
        &self,
    ) -> Option<super::PendingSoftwareProfileOpenStage> {
        self.pending_software_profile_open
            .as_ref()
            .map(PendingSoftwareProfileOpen::stage)
    }

    pub(in crate::root) fn pending_software_profile_open_base_label(&self) -> Option<&str> {
        self.pending_software_profile_open
            .as_ref()
            .map(|pending| pending.base_metadata.label.as_str())
    }

    pub(super) fn pending_software_profile_open_is_current(
        &self,
        operation_generation: u64,
        active_wallet_generation: u64,
        selected_chain: u64,
        base_profile_uuid: &str,
    ) -> bool {
        pending_profile_result_is_current(
            operation_generation,
            self.pending_software_profile_open_operation_generation,
            active_wallet_generation,
            self.active_wallet_generation,
            base_profile_uuid,
            self.pending_software_profile_base_profile_uuid.as_deref(),
            self.selected_wallet_id.as_deref(),
            selected_chain,
            self.selected_chain,
            matches!(self.vault_state, VaultState::PendingSoftwareProfileOpen),
        )
    }

    pub(super) fn abandon_pending_software_profile_open(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
        self.shutdown_wallet_session_store();
        self.clear_protected_software_seed_session(cx);
        clear_wallet_context_visibility(
            &mut self.view_session,
            &mut self.selected_wallet_id,
            &mut self.revealed_passphrase_context_id,
            &mut self.wallet_metadata,
            &mut self.wallet_options,
            &mut self.public_accounts,
        );
        self.vault_view_unlock = None;
        self.private_address_book.clear();
        self.public_address_book.clear();
        self.set_broadcaster_preferences(wallet_ops::vault::BroadcasterPreferences::default(), cx);
        self.broadcaster_preference_error = None;
        self.reset_wallet_scoped_state(cx);
        self.sync_wallet_select(window, cx);
        self.vault_state = VaultState::UnlockVault;
        self.focus_vault_input_on_render = true;
        cx.notify();
    }

    pub(in crate::root) const fn invalidate_pending_profile_open_tokens(&mut self) {
        self.pending_software_profile_open_operation_generation = self
            .pending_software_profile_open_operation_generation
            .wrapping_add(1);
        self.pending_software_profile_open_lifetime_generation = self
            .pending_software_profile_open_lifetime_generation
            .wrapping_add(1);
    }

    pub(in crate::root) fn lock_vault(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !vault_lock_is_allowed(
            self.maintenance_controller.read(cx).is_idle(),
            self.manage_wallets.deleting_wallet_id.is_some(),
        ) {
            return;
        }
        self.shutdown_wallet_session_store();
        self.invalidate_governance_context();
        self.invalidate_proposals_chain(self.selected_chain);
        self.clear_private_broadcaster_progress_state();
        self.stop_waku();
        window.close_all_dialogs(cx);
        self.clear_protected_software_seed_session(cx);
        self.view_session = None;
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
        clear_wallet_context_visibility(
            &mut self.view_session,
            &mut self.selected_wallet_id,
            &mut self.revealed_passphrase_context_id,
            &mut self.wallet_metadata,
            &mut self.wallet_options,
            &mut self.public_accounts,
        );
        self.private_address_book.clear();
        self.public_address_book.clear();
        self.set_broadcaster_preferences(wallet_ops::vault::BroadcasterPreferences::default(), cx);
        self.broadcaster_preference_error = None;
        self.address_book.search_query = Arc::from("");
        self.address_book
            .search_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.address_book.clear_dialog_state(window, cx);
        self.favorite_broadcaster_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.banned_broadcaster_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.active_broadcaster_tab = BroadcasterActivityTab::default();
        self.address_book_save_error = None;
        self.advance_active_wallet_generation();
        self.sync_wallet_select(window, cx);
        self.send_forms.clear();
        self.unshield_forms.clear();
        self.reset_public_wallet_state(window, cx);
        self.private_action_form = None;
        self.broadcaster_picker = None;
        self.active_wallet_tab = WalletTab::default();
        self.setup_password = None;
        self.vault_view_unlock = None;
        self.auto_lock.disarm();
        self.generated_seed = None;
        self.clear_key_export_dialog_state(window, cx);
        #[cfg(feature = "hardware")]
        {
            self.active_hardware_profile = None;
            self.hardware_profile_unlock = HardwareProfileUnlockState::default();
            self.clear_hardware_profile_sensitive_inputs(window, cx);
        }
        self.hardware_wallet_creation_in_progress = false;
        self.hardware_wallet_creation_generation =
            self.hardware_wallet_creation_generation.wrapping_add(1);
        self.hardware_wallet_creation_intent = None;
        self.clear_hardware_wallet_restore_account_index(window, cx);
        self.vault_error = None;
        self.repair_cache_error = None;
        self.vault_state = VaultState::UnlockVault;
        self.wallet_setup_mode = WalletSetupMode::Choose;
        self.focus_vault_input_on_render = true;
        for state in self.chain_states.values_mut() {
            *state = ChainUtxoState::Idle;
        }
        self.sync_utxo_table(cx);
        cx.notify();
    }

    pub(in crate::root) fn handle_vault_error(
        &mut self,
        error: &VaultError,
        cx: &mut Context<'_, Self>,
    ) {
        tracing::warn!(
            error_kind = vault_error_kind(error),
            "desktop wallet vault operation failed"
        );
        self.set_vault_error(vault_error_message(error), cx);
    }

    pub(in crate::root) fn set_vault_error(
        &mut self,
        message: impl Into<Arc<str>>,
        cx: &mut Context<'_, Self>,
    ) {
        self.vault_error = Some(message.into());
        cx.notify();
    }

    fn set_wallet_replacement_error(&mut self, message: &'static str, cx: &mut Context<'_, Self>) {
        self.vault_error = None;
        self.vault_state = VaultState::Error(Arc::from(message));
        self.focus_vault_input_on_render = false;
        cx.notify();
    }
}

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
mod tests {
    use super::{
        clear_wallet_context_visibility, pending_profile_result_is_current,
        pending_software_profile_open_timeout_is_current, wallet_replacement_finalize_is_admitted,
    };
    use crate::root::WalletOption;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use wallet_ops::vault::{
        PublicAccountMetadata, PublicAccountScope, PublicAccountSource, PublicAccountStatus,
        WalletMetadataBundle, WalletSoftwareContext,
    };

    #[test]
    fn pending_profile_timeout_ignores_stale_lifetime_or_state() {
        assert!(pending_software_profile_open_timeout_is_current(7, 7, true));
        assert!(!pending_software_profile_open_timeout_is_current(
            7, 8, true
        ));
        assert!(!pending_software_profile_open_timeout_is_current(
            7, 7, false
        ));
    }

    #[test]
    fn wallet_replacement_finalize_requires_current_switching_admission() {
        assert!(wallet_replacement_finalize_is_admitted(
            7, 7, 1, 1, false, true
        ));
        assert!(!wallet_replacement_finalize_is_admitted(
            6, 7, 1, 1, false, true
        ));
        assert!(!wallet_replacement_finalize_is_admitted(
            7, 7, 2, 1, false, true
        ));
        assert!(!wallet_replacement_finalize_is_admitted(
            7, 7, 1, 1, true, true
        ));
        assert!(!wallet_replacement_finalize_is_admitted(
            7, 7, 1, 1, false, false
        ));
    }

    #[test]
    fn pending_profile_result_rejects_every_replacement_boundary() {
        let current = |operation, wallet, profile, selected, chain, pending| {
            pending_profile_result_is_current(
                operation,
                7,
                wallet,
                11,
                "base-profile",
                profile,
                selected,
                chain,
                1,
                pending,
            )
        };

        assert!(current(7, 11, Some("base-profile"), None, 1, true));
        assert!(!current(6, 11, Some("base-profile"), None, 1, true));
        assert!(!current(7, 10, Some("base-profile"), None, 1, true));
        assert!(!current(7, 11, Some("other-profile"), None, 1, true));
        assert!(!current(
            7,
            11,
            Some("base-profile"),
            Some("selected"),
            1,
            true
        ));
        assert!(!current(7, 11, Some("base-profile"), None, 2, true));
        assert!(!current(7, 11, Some("base-profile"), None, 1, false));
    }

    #[test]
    fn pending_context_cleanup_clears_visible_state() {
        let mut view_session = Some(Arc::new(()));
        let mut selected_wallet_id = Some(Arc::from("child"));
        let mut revealed = Some(Arc::from("child"));
        let mut metadata = vec![WalletMetadataBundle {
            wallet_uuid: "child".to_owned(),
            label: "Child".to_owned(),
            derivation_index: 0,
            source: wallet_ops::vault::WalletSource::Imported,
            status: wallet_ops::vault::WalletStatus::Active,
            display_order: 0,
            hardware_descriptor: None,
            hardware_account: None,
            pending_create_new_chain_ids: BTreeSet::default(),
            software_context: Some(WalletSoftwareContext::passphrase("base")),
        }];
        let mut options = vec![WalletOption {
            wallet_id: Arc::from("child"),
            source: wallet_ops::vault::WalletSource::Imported,
        }];
        let mut accounts = vec![PublicAccountMetadata {
            public_account_uuid: "child-account".to_owned(),
            address: alloy::primitives::Address::ZERO,
            label: None,
            source: PublicAccountSource::Derived,
            scope: PublicAccountScope::PrivateWallet {
                wallet_uuid: "child".to_owned(),
            },
            derivation_index: Some(0),
            hardware_descriptor: None,
            status: PublicAccountStatus::Active,
            display_order: 0,
        }];

        clear_wallet_context_visibility(
            &mut view_session,
            &mut selected_wallet_id,
            &mut revealed,
            &mut metadata,
            &mut options,
            &mut accounts,
        );

        assert!(view_session.is_none());
        assert!(selected_wallet_id.is_none());
        assert!(revealed.is_none());
        assert!(metadata.is_empty());
        assert!(options.is_empty());
        assert!(accounts.is_empty());
    }
}
