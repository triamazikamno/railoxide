#[cfg(not(feature = "hardware"))]
use super::HardwareWalletSyncIntent;
use super::{
    Arc, Context, DesktopViewSession, Focusable, HardwareDeviceKind, PRIMARY_WALLET_LABEL,
    PendingSoftwareProfileOpen, SpendUnlock, VaultError, VaultSessionId, VaultState, ViewUnlock,
    WalletRoot, WalletSetupMode, WalletSoftwareContextKind, WalletSource, Window, Zeroizing,
    default_hardware_wallet_setup_intent, generate_opaque_id, generate_seed_material,
    remembered_wallet_id_for_restore,
};
#[cfg(feature = "hardware")]
use super::{HardwareProfileUnlockPurpose, parse_hardware_wallet_restore_account_index};

struct VaultUnlockResult {
    session: Option<DesktopViewSession>,
    metadata: Vec<wallet_ops::vault::WalletMetadataBundle>,
    vault_view_unlock: Arc<ViewUnlock>,
    setup_password: Option<Zeroizing<String>>,
    pending_software_profile_open: Option<PendingSoftwareProfileOpen>,
    #[cfg(feature = "hardware")]
    remembered_hardware_wallet_id: Option<Arc<str>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum SoftwareProfileRestoreAction {
    Standard,
    Pending,
}

pub(in crate::root) const fn software_profile_restore_action(
    auto_open_standard_context: bool,
) -> SoftwareProfileRestoreAction {
    if auto_open_standard_context {
        SoftwareProfileRestoreAction::Standard
    } else {
        SoftwareProfileRestoreAction::Pending
    }
}

impl VaultUnlockResult {
    const fn new(
        session: Option<DesktopViewSession>,
        metadata: Vec<wallet_ops::vault::WalletMetadataBundle>,
        vault_view_unlock: Arc<ViewUnlock>,
        setup_password: Option<Zeroizing<String>>,
    ) -> Self {
        Self {
            session,
            metadata,
            vault_view_unlock,
            setup_password,
            pending_software_profile_open: None,
            #[cfg(feature = "hardware")]
            remembered_hardware_wallet_id: None,
        }
    }

    #[cfg(feature = "hardware")]
    const fn remembered_hardware(
        wallet_id: Arc<str>,
        metadata: Vec<wallet_ops::vault::WalletMetadataBundle>,
        vault_view_unlock: Arc<ViewUnlock>,
    ) -> Self {
        Self {
            session: None,
            metadata,
            vault_view_unlock,
            setup_password: None,
            pending_software_profile_open: None,
            remembered_hardware_wallet_id: Some(wallet_id),
        }
    }
}

fn new_pending_software_profile_open(
    vault_view_unlock: &Arc<ViewUnlock>,
    metadata: Vec<wallet_ops::vault::WalletMetadataBundle>,
    base_metadata: wallet_ops::vault::WalletMetadataBundle,
    spend_unlock: SpendUnlock,
) -> Result<PendingSoftwareProfileOpen, VaultError> {
    let vault_session_id = VaultSessionId::random()?;
    PendingSoftwareProfileOpen::new(
        base_metadata,
        metadata,
        Arc::clone(vault_view_unlock),
        spend_unlock,
        vault_session_id,
        0,
    )
    .ok_or(VaultError::InvalidWalletMetadata)
}

fn load_software_profile_unlock_target(
    store: &wallet_ops::vault::DesktopVaultStore,
    vault_view_unlock: &Arc<ViewUnlock>,
    metadata: &[wallet_ops::vault::WalletMetadataBundle],
    remembered_wallet_id: Option<&str>,
    password: &Zeroizing<String>,
) -> Result<VaultUnlockResult, VaultError> {
    let mut candidates = metadata
        .iter()
        .filter(|metadata| {
            metadata.status == wallet_ops::vault::WalletStatus::Active
                && !metadata.source.is_hardware_derived()
                && metadata.software_context.as_ref().is_some_and(|context| {
                    context.kind == WalletSoftwareContextKind::Standard
                        && context.base_profile_uuid == metadata.wallet_uuid
                })
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|metadata| {
        u8::from(remembered_wallet_id != Some(metadata.wallet_uuid.as_str()))
    });

    for base_metadata in candidates {
        let session = match store.load_view_session_with_view_unlock(
            vault_view_unlock.as_ref(),
            &base_metadata.wallet_uuid,
        ) {
            Ok(session) => session,
            Err(error) => {
                tracing::warn!(
                    %error,
                    wallet_id = base_metadata.wallet_uuid,
                    "software profile could not be restored; falling back"
                );
                continue;
            }
        };
        let auto_open = base_metadata
            .software_context
            .as_ref()
            .is_some_and(|context| context.auto_open_standard_context);
        let mut grant = store.create_spend_grant(password.as_str())?;
        let spend_unlock = grant.take_spend_unlock()?;
        if software_profile_restore_action(auto_open) == SoftwareProfileRestoreAction::Standard {
            drop(spend_unlock);
            return Ok(VaultUnlockResult::new(
                Some(session),
                metadata.to_vec(),
                Arc::clone(vault_view_unlock),
                None,
            ));
        }
        let pending = new_pending_software_profile_open(
            vault_view_unlock,
            metadata.to_vec(),
            base_metadata.clone(),
            spend_unlock,
        )?;
        return Ok(VaultUnlockResult {
            session: None,
            metadata: metadata.to_vec(),
            vault_view_unlock: Arc::clone(vault_view_unlock),
            setup_password: None,
            pending_software_profile_open: Some(pending),
            #[cfg(feature = "hardware")]
            remembered_hardware_wallet_id: None,
        });
    }

    Ok(VaultUnlockResult::new(
        None,
        metadata.to_vec(),
        Arc::clone(vault_view_unlock),
        None,
    ))
}

impl WalletRoot {
    pub(in crate::root) fn submit_wallet_setup_from_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match self.wallet_setup_mode {
            WalletSetupMode::Import => self.store_imported_wallet(window, cx),
            WalletSetupMode::Hardware(_) => self.submit_default_hardware_wallet_setup(window, cx),
            WalletSetupMode::Choose | WalletSetupMode::GeneratedReview => {}
        }
    }

    pub(in crate::root) fn create_vault_from_inputs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.as_ref() else {
            self.set_vault_error("Wallet vault storage is unavailable", cx);
            return;
        };
        let password = Self::read_and_clear_input(&self.new_password_input, window, cx);
        let confirm = Self::read_and_clear_input(&self.confirm_password_input, window, cx);

        if password.trim().is_empty() {
            self.set_vault_error("Enter a vault password to continue", cx);
            return;
        }
        if password.as_str() != confirm.as_str() {
            self.set_vault_error("Vault passwords do not match", cx);
            return;
        }

        match store.create_vault(password.as_str()) {
            Ok(created) => {
                Self::defer_wallet_name_input(PRIMARY_WALLET_LABEL.to_owned(), window, cx);
                self.install_vault_view_unlock(Arc::new(created.view));
                self.setup_password = Some(password);
                self.vault_error = None;
                self.vault_state = VaultState::SetupWallet;
                self.wallet_setup_mode = WalletSetupMode::Choose;
                self.ensure_waku_started(cx);
                cx.notify();
            }
            Err(VaultError::VaultAlreadyExists) => {
                self.vault_state = VaultState::UnlockVault;
                self.focus_vault_input_on_render = true;
                self.set_vault_error("A wallet vault already exists. Unlock it to continue.", cx);
            }
            Err(error) => self.handle_vault_error(&error, cx),
        }
    }

    pub(in crate::root) fn unlock_vault_from_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let password = Self::read_and_clear_input(&self.unlock_password_input, window, cx);
        self.unlock_vault_with_password(password, None, window, cx);
    }

    pub(in crate::root) fn unlock_vault_with_password(
        &mut self,
        password: Zeroizing<String>,
        remote: Option<wallet_ops::gateway::GatewayUnlockGuard>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.unlock_in_progress {
            return;
        }
        let Some(store) = self.vault_store.as_ref() else {
            self.set_vault_error("Wallet vault storage is unavailable", cx);
            return;
        };
        if password.trim().is_empty() {
            self.set_vault_error("Enter the vault password to continue", cx);
            return;
        }

        let store = Arc::clone(store);
        let gateway_unlock = self.begin_gateway_unlock();
        self.set_remote_unlock_attempt(
            remote
                .as_ref()
                .map(wallet_ops::gateway::GatewayUnlockGuard::attempt),
        );
        let remembered_wallet_id = self.ui_state.last_wallet_id.clone();
        let remembered_wallet_kind = self.ui_state.last_wallet_kind;
        let active_wallet_generation = self.active_wallet_generation;
        self.unlock_in_progress = true;
        self.vault_error = None;
        cx.notify();

        let join = self.runtime.spawn_blocking(move || {
            let view = store.unlock_view(password.as_str())?;
            let metadata = store.list_wallet_metadata_with_view_unlock(&view, true)?;
            let remembered_wallet_id = remembered_wallet_id_for_restore(
                &metadata,
                remembered_wallet_id.as_deref(),
                remembered_wallet_kind,
            );
            let has_active_wallet = metadata
                .iter()
                .any(|metadata| metadata.status == wallet_ops::vault::WalletStatus::Active);
            let vault_view_unlock = Arc::new(view);
            if !has_active_wallet {
                return Ok(VaultUnlockResult::new(
                    None,
                    metadata,
                    vault_view_unlock,
                    Some(password),
                ));
            }
            if let Some(wallet) = metadata.iter().find(|metadata| {
                metadata.status == wallet_ops::vault::WalletStatus::Active
                    && Some(metadata.wallet_uuid.as_str()) == remembered_wallet_id.as_deref()
            }) && wallet.source.is_hardware_derived()
            {
                #[cfg(feature = "hardware")]
                {
                    return Ok(VaultUnlockResult::remembered_hardware(
                        Arc::from(wallet.wallet_uuid.as_str()),
                        metadata,
                        vault_view_unlock,
                    ));
                }
                #[cfg(not(feature = "hardware"))]
                {
                    tracing::warn!(
                        wallet_id = wallet.wallet_uuid.as_str(),
                        "remembered hardware wallet cannot be opened in this build; falling back"
                    );
                }
            }
            load_software_profile_unlock_target(
                &store,
                &vault_view_unlock,
                &metadata,
                remembered_wallet_id.as_deref(),
                &password,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.unlock_in_progress = false;
                if remote.as_ref().is_some_and(|guard| !guard.is_current())
                    || root.active_wallet_generation != active_wallet_generation
                {
                    return;
                }
                match result {
                    Ok(Ok(unlock)) if unlock.session.is_some() => {
                        if let Some(guard) = &remote {
                            guard.finish(wallet_ops::gateway::GatewayUnlockPhase::Opening);
                        }
                        if remote.is_none() {
                            root.install_vault_view_unlock(unlock.vault_view_unlock);
                        }
                        root.install_view_session_for_gateway_unlock(
                            unlock.session.expect("checked above"),
                            &unlock.metadata,
                            gateway_unlock,
                            window,
                            cx,
                        );
                    }
                    Ok(Ok(unlock)) => {
                        if let Some(guard) = &remote {
                            if unlock.pending_software_profile_open.is_some() {
                                guard.finish(wallet_ops::gateway::GatewayUnlockPhase::Passphrase);
                            } else {
                                if !guard.attempt().finish_if_current(
                                    wallet_ops::gateway::GatewayUnlockPhase::Desktop,
                                ) {
                                    return;
                                }
                                root.set_remote_unlock_attempt(None);
                            }
                        }
                        root.enter_password_metadata_unlocked(
                            &unlock.metadata,
                            unlock.vault_view_unlock,
                            unlock.setup_password,
                            unlock.pending_software_profile_open,
                            gateway_unlock,
                            window,
                            cx,
                        );
                        #[cfg(feature = "hardware")]
                        if let Some(wallet_id) = unlock.remembered_hardware_wallet_id {
                            root.vault_error = None;
                            root.open_hardware_profile_unlock_dialog_for_wallet_continuing(
                                wallet_id,
                                gateway_unlock,
                                window,
                                cx,
                            );
                        }
                    }
                    Ok(Err(error)) => {
                        if let Some(guard) = &remote {
                            guard.finish(wallet_ops::gateway::GatewayUnlockPhase::Failed);
                        }
                        if root.gateway_unlock_is_current(gateway_unlock) {
                            root.retire_gateway_unlock();
                        }
                        root.focus_vault_input_on_render = true;
                        root.handle_vault_error(&error, cx);
                    }
                    Err(error) => {
                        if let Some(guard) = &remote {
                            guard.finish(wallet_ops::gateway::GatewayUnlockPhase::Failed);
                        }
                        if root.gateway_unlock_is_current(gateway_unlock) {
                            root.retire_gateway_unlock();
                        }
                        tracing::warn!(%error, "desktop wallet vault unlock task failed");
                        root.focus_vault_input_on_render = true;
                        root.set_vault_error(
                            "Unlock failed. Check the password and try again.",
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
    }

    pub(in crate::root) fn choose_generated_wallet(&mut self, cx: &mut Context<'_, Self>) {
        match generate_seed_material() {
            Ok(seed) => {
                self.generated_seed = Some(seed);
                self.vault_error = None;
                self.wallet_setup_mode = WalletSetupMode::GeneratedReview;
                cx.notify();
            }
            Err(error) => self.handle_vault_error(&error, cx),
        }
    }

    pub(in crate::root) fn choose_import_wallet(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.generated_seed = None;
        self.vault_error = None;
        self.wallet_setup_mode = WalletSetupMode::Import;
        cx.notify();
        cx.defer_in(window, move |root, window, cx| {
            if root.wallet_setup_mode == WalletSetupMode::Import {
                root.import_mnemonic_input
                    .read(cx)
                    .focus_handle(cx)
                    .focus(window, cx);
            }
        });
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn choose_hardware_wallet(
        &mut self,
        device_kind: HardwareDeviceKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_hardware_profile_unlock_dialog_for_device(
            device_kind,
            HardwareProfileUnlockPurpose::Add,
            window,
            cx,
        );
    }

    #[cfg(not(feature = "hardware"))]
    pub(in crate::root) fn choose_hardware_wallet(
        &mut self,
        device_kind: HardwareDeviceKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.generated_seed = None;
        self.vault_error = None;
        self.wallet_setup_mode = WalletSetupMode::Hardware(device_kind);
        self.hardware_wallet_creation_intent = None;
        self.clear_hardware_wallet_restore_account_index(window, cx);
        cx.notify();
        cx.defer_in(window, move |root, window, cx| {
            if matches!(root.vault_state, VaultState::ViewUnlocked)
                && root.wallet_setup_mode == WalletSetupMode::Hardware(device_kind)
            {
                root.add_wallet_password_input
                    .read(cx)
                    .focus_handle(cx)
                    .focus(window, cx);
            }
        });
    }

    pub(in crate::root) fn submit_default_hardware_wallet_setup(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let WalletSetupMode::Hardware(device_kind) = self.wallet_setup_mode else {
            return;
        };
        self.store_hardware_derived_wallet(
            device_kind,
            default_hardware_wallet_setup_intent(
                self.hardware_wallet_creation_intent,
                self.hardware_wallet_restore_account_index_set,
            ),
            window,
            cx,
        );
    }

    pub(in crate::root) fn back_to_wallet_setup_choice(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.generated_seed = None;
        self.import_mnemonic_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.vault_error = None;
        self.wallet_setup_mode = WalletSetupMode::Choose;
        self.hardware_wallet_creation_intent = None;
        self.clear_hardware_wallet_restore_account_index(window, cx);
        cx.notify();
    }

    pub(super) fn wallet_creation_password(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<Zeroizing<String>> {
        if matches!(self.vault_state, VaultState::ViewUnlocked) {
            let password = Self::read_and_clear_input(&self.add_wallet_password_input, window, cx);
            if password.trim().is_empty() {
                self.set_vault_error("Enter the vault password to add a wallet", cx);
                return None;
            }
            return Some(password);
        }
        let Some(password) = self.setup_password.as_ref() else {
            self.set_vault_error("Unlock the wallet vault before adding a wallet", cx);
            return None;
        };
        Some(Zeroizing::new(password.to_string()))
    }

    #[cfg(feature = "hardware")]
    pub(super) fn hardware_wallet_creation_password(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Option<Zeroizing<String>> {
        if matches!(self.vault_state, VaultState::ViewUnlocked) {
            let password =
                Zeroizing::new(self.add_wallet_password_input.read(cx).value().to_string());
            if password.trim().is_empty() {
                self.set_vault_error("Enter the vault password to add a wallet", cx);
                return None;
            }
            return Some(password);
        }
        let Some(password) = self.setup_password.as_ref() else {
            self.set_vault_error("Unlock the wallet vault before adding a wallet", cx);
            return None;
        };
        Some(Zeroizing::new(password.to_string()))
    }

    #[cfg(feature = "hardware")]
    #[allow(clippy::option_option)]
    pub(super) fn hardware_wallet_restore_account_index(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Option<Option<u32>> {
        let value = self
            .hardware_wallet_restore_account_index_input
            .read(cx)
            .value()
            .to_string();
        match parse_hardware_wallet_restore_account_index(&value) {
            Ok(index) => Some(index),
            Err(message) => {
                self.set_vault_error(message, cx);
                None
            }
        }
    }

    pub(super) fn wallet_name_from_input(&self, cx: &Context<'_, Self>) -> String {
        self.wallet_name_input.read(cx).value().to_string()
    }

    pub(in crate::root) fn store_generated_wallet(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let wallet_id = match generate_opaque_id() {
            Ok(wallet_id) => wallet_id,
            Err(error) => {
                self.handle_vault_error(&error, cx);
                return;
            }
        };
        let label = self.wallet_name_from_input(cx);
        let pending_create_new_chain_ids = self.enabled_chain_ids_for_created_wallet();
        let Some(password) = self.wallet_creation_password(window, cx) else {
            return;
        };
        let result = {
            let Some(store) = self.vault_store.as_ref() else {
                self.set_vault_error("Wallet vault storage is unavailable", cx);
                return;
            };
            let Some(seed) = self.generated_seed.as_ref() else {
                self.set_vault_error("Generate a recovery phrase before creating the wallet", cx);
                return;
            };
            let metadata = store.new_wallet_metadata_with_pending_create_new_chain_ids(
                password.as_str(),
                &wallet_id,
                0,
                WalletSource::Generated,
                &label,
                pending_create_new_chain_ids,
            );
            let metadata = match metadata {
                Ok(metadata) => metadata,
                Err(error) => return self.handle_vault_error(&error, cx),
            };
            store
                .store_generated_wallet_with_metadata(
                    password.as_str(),
                    &wallet_id,
                    0,
                    "english",
                    seed,
                    &metadata,
                )
                .and_then(|_| {
                    let metadata = store.list_wallet_metadata(password.as_str())?;
                    let session = store.load_view_session(password.as_str(), &wallet_id)?;
                    Ok((session, metadata))
                })
        };

        match result {
            Ok((session, metadata)) => {
                self.enter_new_wallet_view_unlocked(session, &metadata, window, cx);
            }
            Err(error) => self.handle_vault_error(&error, cx),
        }
    }

    pub(in crate::root) fn store_imported_wallet(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let mnemonic = Zeroizing::new(self.import_mnemonic_input.read(cx).value().to_string());
        self.import_mnemonic_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        if mnemonic.trim().is_empty() {
            self.set_vault_error("Paste a recovery phrase to import", cx);
            return;
        }
        let wallet_id = match generate_opaque_id() {
            Ok(wallet_id) => wallet_id,
            Err(error) => {
                self.handle_vault_error(&error, cx);
                return;
            }
        };
        let label = self.wallet_name_from_input(cx);
        let Some(password) = self.wallet_creation_password(window, cx) else {
            return;
        };

        let result = {
            let Some(store) = self.vault_store.as_ref() else {
                self.set_vault_error("Wallet vault storage is unavailable", cx);
                return;
            };
            let metadata = store.new_wallet_metadata(
                password.as_str(),
                &wallet_id,
                0,
                WalletSource::Imported,
                &label,
            );
            let metadata = match metadata {
                Ok(metadata) => metadata,
                Err(error) => return self.handle_vault_error(&error, cx),
            };
            store
                .import_wallet_mnemonic_with_metadata(
                    password.as_str(),
                    &wallet_id,
                    0,
                    "english",
                    mnemonic.as_str(),
                    &metadata,
                )
                .and_then(|_| {
                    let metadata = store.list_wallet_metadata(password.as_str())?;
                    let session = store.load_view_session(password.as_str(), &wallet_id)?;
                    Ok((session, metadata))
                })
        };

        match result {
            Ok((session, metadata)) => self.enter_view_unlocked(session, &metadata, window, cx),
            Err(error) => self.handle_vault_error(&error, cx),
        }
    }

    #[cfg(not(feature = "hardware"))]
    pub(in crate::root) fn store_hardware_derived_wallet(
        &mut self,
        _device_kind: HardwareDeviceKind,
        sync_intent: HardwareWalletSyncIntent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.hardware_wallet_creation_intent = Some(sync_intent);
        self.set_vault_error(
            "Hardware wallet support is not enabled in this build. Rebuild the wallet with the hardware feature to use Ledger-derived or Trezor-derived wallets.",
            cx,
        );
    }
}
