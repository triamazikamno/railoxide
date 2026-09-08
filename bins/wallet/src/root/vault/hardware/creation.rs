#[cfg(feature = "hardware")]
use std::collections::BTreeSet;

#[cfg(not(feature = "hardware"))]
use super::WalletRoot;
#[cfg(feature = "hardware")]
use super::{
    Arc, Context, DEFAULT_HARDWARE_DERIVATION_PATH, DesktopVaultStore, DesktopViewSession,
    Focusable, HARDWARE_PROFILE_READINESS_RETRY_INTERVAL, HardwareDerivationDescriptor,
    HardwareDeviceKind, HardwareProfileMetadata, HardwareProfileProgressUpdate,
    HardwareProfileSession, HardwareProfileStep, HardwareProfileStepStatus,
    HardwareRailgunAccountMetadata, HardwareSetupErrorFocus, HardwareViewAccessKey,
    HardwareWalletCreationError, HardwareWalletCreationResult, HardwareWalletProfile,
    HardwareWalletSyncIntent, LedgerHardwareDerivationClient, SyntheticRailgunEntropy,
    TrezorHardwareDerivationClient, TrezorPassphraseMode, VaultError, VaultState, ViewUnlock,
    WalletMetadataBundle, WalletRoot, WalletSetupMode, Window, Zeroizing,
    default_hardware_profile_label, effective_trezor_passphrase_mode, generate_opaque_id,
    hardware_approval_error, hardware_profile_approval_prompt_for_descriptor,
    hardware_profile_detection_should_retry,
    hardware_profile_detection_should_suppress_initial_ledger_progress,
    hardware_profile_detection_should_suppress_initial_trezor_progress,
    hardware_setup_error_preserves_password, hardware_setup_vault_error_focus,
    hardware_setup_vault_error_message, hardware_setup_vault_error_preserves_password,
    hardware_view_access_key_from_hardware_output, mpsc, parse_bip32_path,
    send_hardware_profile_approval_progress, send_hardware_profile_progress,
    send_hardware_profile_readiness_progress, send_trezor_passphrase_policy_progress, sleep,
    synthetic_entropy_from_hardware_output, trezor_pin_matrix_provider, vault_error_kind,
};

#[cfg(any(feature = "hardware", test))]
pub(in crate::root) const fn hardware_wallet_creation_result_is_current(
    current_generation: u64,
    result_generation: u64,
) -> bool {
    current_generation == result_generation
}

impl WalletRoot {
    #[cfg(feature = "hardware")]
    pub(in crate::root) fn store_hardware_derived_wallet(
        &mut self,
        device_kind: HardwareDeviceKind,
        sync_intent: HardwareWalletSyncIntent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.hardware_wallet_creation_in_progress {
            return;
        }
        self.hardware_wallet_creation_intent = Some(sync_intent);
        let Some(explicit_account_index) = self.hardware_wallet_restore_account_index(cx) else {
            return;
        };
        if sync_intent == HardwareWalletSyncIntent::CreateNew && explicit_account_index.is_some() {
            self.set_vault_error(
                "Clear the restore account index before creating a new hardware-derived wallet.",
                cx,
            );
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
        let Some(password) = self.hardware_wallet_creation_password(cx) else {
            return;
        };
        let Some(store) = self.vault_store.clone() else {
            self.set_vault_error("Wallet vault storage is unavailable", cx);
            return;
        };
        let label = match store.preflight_new_wallet_metadata(password.as_str(), &label) {
            Ok(label) => label,
            Err(error) => {
                self.handle_hardware_wallet_setup_vault_error(&error, &label, window, cx);
                return;
            }
        };
        let pending_create_new_chain_ids = self.enabled_chain_ids_for_created_wallet();

        window.blur(cx);
        self.focus_vault_input_on_render = false;
        self.hardware_wallet_creation_in_progress = true;
        self.hardware_wallet_creation_generation =
            self.hardware_wallet_creation_generation.wrapping_add(1);
        let creation_generation = self.hardware_wallet_creation_generation;
        self.vault_error = None;
        cx.notify();

        let join = self.runtime.spawn(create_hardware_derived_wallet(
            store,
            password,
            wallet_id,
            label,
            device_kind,
            sync_intent,
            explicit_account_index,
            pending_create_new_chain_ids,
        ));
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if !hardware_wallet_creation_result_is_current(
                    root.hardware_wallet_creation_generation,
                    creation_generation,
                ) {
                    return;
                }
                root.hardware_wallet_creation_in_progress = false;
                match result {
                    Ok(Ok((session, metadata))) => {
                        if sync_intent == HardwareWalletSyncIntent::CreateNew {
                            root.enter_new_wallet_view_unlocked(session, &metadata, window, cx);
                        } else {
                            root.enter_view_unlocked(session, &metadata, window, cx);
                        }
                    }
                    Ok(Err(HardwareWalletCreationError::Vault(error))) => {
                        root.handle_hardware_wallet_setup_vault_error(&error, "", window, cx);
                    }
                    Ok(Err(HardwareWalletCreationError::Hardware { error, .. })) => {
                        let clear_password = !hardware_setup_error_preserves_password(&error);
                        root.set_vault_error(
                            format!("Hardware wallet derivation failed: {error}"),
                            cx,
                        );
                        root.finish_hardware_wallet_setup_error(
                            window,
                            cx,
                            clear_password,
                            HardwareSetupErrorFocus::VaultPassword,
                        );
                    }
                    Err(error) => {
                        tracing::warn!(%error, "desktop hardware wallet setup task failed");
                        root.set_vault_error(
                            "Hardware wallet setup failed. See logs for non-sensitive diagnostics.",
                            cx,
                        );
                        root.finish_hardware_wallet_setup_error(
                            window,
                            cx,
                            true,
                            HardwareSetupErrorFocus::VaultPassword,
                        );
                    }
                }
            });
        })
        .detach();
    }

    #[cfg(feature = "hardware")]
    fn handle_hardware_wallet_setup_vault_error(
        &mut self,
        error: &VaultError,
        label: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        tracing::warn!(
            error_kind = vault_error_kind(error),
            "desktop wallet vault operation failed"
        );
        self.set_vault_error(hardware_setup_vault_error_message(error, label), cx);
        self.finish_hardware_wallet_setup_error(
            window,
            cx,
            !hardware_setup_vault_error_preserves_password(error),
            hardware_setup_vault_error_focus(error),
        );
    }

    #[cfg(feature = "hardware")]
    fn finish_hardware_wallet_setup_error(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
        clear_password: bool,
        focus: HardwareSetupErrorFocus,
    ) {
        if !matches!(self.vault_state, VaultState::ViewUnlocked) {
            return;
        }
        if clear_password {
            self.add_wallet_password_input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.defer_in(window, move |root, window, cx| {
            if matches!(root.vault_state, VaultState::ViewUnlocked)
                && matches!(root.wallet_setup_mode, WalletSetupMode::Hardware(_))
                && !root.hardware_wallet_creation_in_progress
            {
                match focus {
                    HardwareSetupErrorFocus::WalletName => root
                        .wallet_name_input
                        .read(cx)
                        .focus_handle(cx)
                        .focus(window, cx),
                    HardwareSetupErrorFocus::VaultPassword => root
                        .add_wallet_password_input
                        .read(cx)
                        .focus_handle(cx)
                        .focus(window, cx),
                }
            }
        });
    }
}

#[cfg(feature = "hardware")]
pub(super) async fn unlock_hardware_profile(
    store: Arc<DesktopVaultStore>,
    vault_view_unlock: Arc<ViewUnlock>,
    device_kind: HardwareDeviceKind,
    trezor_mode: TrezorPassphraseMode,
    trezor_app_passphrase: Option<Zeroizing<String>>,
    progress_tx: mpsc::UnboundedSender<HardwareProfileProgressUpdate>,
) -> Result<
    (
        Arc<ViewUnlock>,
        HardwareProfileSession,
        HardwareProfileMetadata,
        Vec<WalletMetadataBundle>,
    ),
    HardwareWalletCreationError,
> {
    let path = parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH)?;
    let mut suppress_initial_ledger_progress = false;
    let mut suppress_initial_trezor_progress = false;
    loop {
        let detection = detect_hardware_profile_once(
            store.as_ref(),
            &vault_view_unlock,
            device_kind,
            trezor_mode,
            trezor_app_passphrase.as_ref(),
            &path,
            suppress_initial_ledger_progress,
            suppress_initial_trezor_progress,
            &progress_tx,
        )
        .await;
        match detection {
            Ok((session, profile, metadata)) => {
                let _ = send_hardware_profile_progress(
                    &progress_tx,
                    HardwareProfileStep::UnlockDevice,
                    HardwareProfileStepStatus::Done,
                    None,
                );
                let _ = send_hardware_profile_progress(
                    &progress_tx,
                    HardwareProfileStep::OpenEthereumApp,
                    HardwareProfileStepStatus::Done,
                    None,
                );
                return Ok((vault_view_unlock, session, profile, metadata));
            }
            Err(HardwareWalletCreationError::Hardware {
                error,
                awaiting_approval,
            }) if !awaiting_approval && hardware_profile_detection_should_retry(&error) => {
                if !send_hardware_profile_readiness_progress(device_kind, &error, &progress_tx) {
                    return Err(HardwareWalletCreationError::Hardware {
                        error,
                        awaiting_approval,
                    });
                }
                suppress_initial_ledger_progress = device_kind == HardwareDeviceKind::Ledger
                    && hardware_profile_detection_should_suppress_initial_ledger_progress(&error);
                suppress_initial_trezor_progress = device_kind == HardwareDeviceKind::Trezor
                    && hardware_profile_detection_should_suppress_initial_trezor_progress(&error);
                sleep(HARDWARE_PROFILE_READINESS_RETRY_INTERVAL).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(feature = "hardware")]
async fn detect_hardware_profile_once(
    store: &DesktopVaultStore,
    vault_view_unlock: &ViewUnlock,
    device_kind: HardwareDeviceKind,
    trezor_mode: TrezorPassphraseMode,
    trezor_app_passphrase: Option<&Zeroizing<String>>,
    path: &[u32],
    suppress_initial_ledger_progress: bool,
    suppress_initial_trezor_progress: bool,
    progress_tx: &mpsc::UnboundedSender<HardwareProfileProgressUpdate>,
) -> Result<
    (
        HardwareProfileSession,
        HardwareProfileMetadata,
        Vec<WalletMetadataBundle>,
    ),
    HardwareWalletCreationError,
> {
    let (profile_fingerprint, trezor_session_id, effective_trezor_mode) = match device_kind {
        HardwareDeviceKind::Ledger => {
            if !suppress_initial_ledger_progress {
                let _ = send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::UnlockDevice,
                    HardwareProfileStepStatus::Pending,
                    Some("Plug it in and enter your PIN."),
                );
            }
            let client = LedgerHardwareDerivationClient::connect().await?;
            let fingerprint = client.profile_fingerprint(path).await?;
            let _ = send_hardware_profile_progress(
                progress_tx,
                HardwareProfileStep::UnlockDevice,
                HardwareProfileStepStatus::Done,
                None,
            );
            let _ = send_hardware_profile_progress(
                progress_tx,
                HardwareProfileStep::OpenEthereumApp,
                HardwareProfileStepStatus::Pending,
                Some("Open the Ethereum app on your Ledger."),
            );
            (fingerprint, None, trezor_mode)
        }
        HardwareDeviceKind::Trezor => {
            if !suppress_initial_trezor_progress {
                let _ = send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::UnlockDevice,
                    HardwareProfileStepStatus::Pending,
                    Some("Plug it in and enter your PIN."),
                );
            }
            let mut client = TrezorHardwareDerivationClient::connect()?;
            client.set_pin_matrix_provider(trezor_pin_matrix_provider(progress_tx.clone()));
            let info = client.device_info()?;
            let _ = send_trezor_passphrase_policy_progress(
                progress_tx,
                info.passphrase_always_on_device,
            );
            let effective_mode =
                effective_trezor_passphrase_mode(trezor_mode, info.passphrase_always_on_device);
            client.set_passphrase_mode(effective_mode);
            if effective_mode == TrezorPassphraseMode::EnterInApp
                && let Some(passphrase) = trezor_app_passphrase.cloned()
            {
                client.set_app_passphrase_zeroizing(passphrase);
            }
            if info.unlocked != Some(false) {
                let _ = send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::UnlockDevice,
                    HardwareProfileStepStatus::Done,
                    None,
                );
                let _ = send_hardware_profile_progress(
                    progress_tx,
                    HardwareProfileStep::OpenEthereumApp,
                    HardwareProfileStepStatus::Pending,
                    Some("Confirm the wallet on your Trezor."),
                );
            }
            let fingerprint = client.profile_fingerprint(path)?;
            (fingerprint, client.session_id(), effective_mode)
        }
    };
    let mut session = store.hardware_profile_session_for_fingerprint_with_view_unlock(
        vault_view_unlock,
        device_kind,
        &profile_fingerprint,
        trezor_session_id.as_deref(),
    )?;
    if device_kind == HardwareDeviceKind::Trezor {
        session.set_trezor_passphrase_mode(effective_trezor_mode);
    }
    let profiles = store.list_hardware_profile_metadata_with_view_unlock(vault_view_unlock)?;
    let profile = session.profile_id.as_ref().map_or_else(
        || {
            HardwareProfileMetadata::from_binding(
                device_kind,
                default_hardware_profile_label(device_kind),
                session.binding.clone(),
            )
        },
        |profile_id| {
            profiles
                .iter()
                .find(|profile| profile.profile_id == *profile_id)
                .cloned()
                .unwrap_or_else(|| {
                    HardwareProfileMetadata::from_binding(
                        device_kind,
                        default_hardware_profile_label(device_kind),
                        session.binding.clone(),
                    )
                })
        },
    );
    let metadata = store.list_wallet_metadata_with_view_unlock(vault_view_unlock, true)?;
    Ok((session, profile, metadata))
}

#[cfg(feature = "hardware")]
pub(super) async fn open_hardware_account(
    store: Arc<DesktopVaultStore>,
    vault_view_unlock: Arc<ViewUnlock>,
    mut hardware_session: HardwareProfileSession,
    wallet_id: String,
    account: HardwareRailgunAccountMetadata,
    trezor_mode: TrezorPassphraseMode,
    trezor_app_passphrase: Option<Zeroizing<String>>,
    progress_tx: mpsc::UnboundedSender<HardwareProfileProgressUpdate>,
) -> Result<HardwareWalletCreationResult, HardwareWalletCreationError> {
    hardware_session.verify_account(&account)?;
    let descriptor = account.descriptor.clone();
    let output = match descriptor.device_kind {
        HardwareDeviceKind::Ledger => {
            let client = LedgerHardwareDerivationClient::connect().await?;
            let active = client.active_profile_session(&descriptor.path).await?;
            active.verify_descriptor(&descriptor)?;
            let _ = send_hardware_profile_approval_progress(
                HardwareDeviceKind::Ledger,
                hardware_profile_approval_prompt_for_descriptor(&descriptor),
                &progress_tx,
            );
            client
                .eip1024_shared_secret(&descriptor.path, true)
                .await
                .map_err(hardware_approval_error)?
        }
        HardwareDeviceKind::Trezor => {
            let mut client = TrezorHardwareDerivationClient::connect_with_session(
                hardware_session.trezor_session_id.clone(),
            )?;
            client.set_pin_matrix_provider(trezor_pin_matrix_provider(progress_tx.clone()));
            let info = client.device_info()?;
            let _ = send_trezor_passphrase_policy_progress(
                &progress_tx,
                info.passphrase_always_on_device,
            );
            let effective_mode =
                effective_trezor_passphrase_mode(trezor_mode, info.passphrase_always_on_device);
            client.set_passphrase_mode(effective_mode);
            if effective_mode == TrezorPassphraseMode::EnterInApp
                && let Some(passphrase) = trezor_app_passphrase
            {
                client.set_app_passphrase_zeroizing(passphrase);
            }
            let active = client.active_profile_session(&descriptor.path)?;
            active.verify_descriptor(&descriptor)?;
            hardware_session.trezor_session_id = active.trezor_session_id;
            hardware_session.set_trezor_passphrase_mode(effective_mode);
            let _ = send_hardware_profile_approval_progress(
                HardwareDeviceKind::Trezor,
                hardware_profile_approval_prompt_for_descriptor(&descriptor),
                &progress_tx,
            );
            client
                .cipher_key_value(&descriptor)
                .map_err(hardware_approval_error)?
        }
    };
    let view_access_key = hardware_view_access_key_from_hardware_output(&descriptor, &output)?;
    let session = store.load_hardware_view_session_with_view_unlock(
        &vault_view_unlock,
        &hardware_session,
        &wallet_id,
        &view_access_key,
    )?;
    let metadata = store.list_wallet_metadata_with_view_unlock(&vault_view_unlock, true)?;
    Ok((session, metadata))
}

#[cfg(feature = "hardware")]
pub(super) async fn create_hardware_profile_accounts(
    store: Arc<DesktopVaultStore>,
    vault_view_unlock: Arc<ViewUnlock>,
    label_prefix: String,
    device_kind: HardwareDeviceKind,
    mut hardware_session: HardwareProfileSession,
    account_indices: Vec<u32>,
    sync_intent: HardwareWalletSyncIntent,
    pending_create_new_chain_ids: BTreeSet<u64>,
    trezor_mode: TrezorPassphraseMode,
    trezor_app_passphrase: Option<Zeroizing<String>>,
    progress_tx: mpsc::UnboundedSender<HardwareProfileProgressUpdate>,
) -> Result<HardwareWalletCreationResult, HardwareWalletCreationError> {
    let path = parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH)?;
    let profile_fingerprint = hardware_session.binding.fingerprint.clone();
    let mut profile_metadata = store
        .list_hardware_profile_metadata_with_view_unlock(&vault_view_unlock)?
        .into_iter()
        .find(|profile| hardware_session.matches_profile(profile))
        .unwrap_or_else(|| {
            HardwareProfileMetadata::from_binding(
                device_kind,
                label_prefix.clone(),
                hardware_session.binding.clone(),
            )
        });
    profile_metadata.label.clone_from(&label_prefix);
    hardware_session.profile_id = Some(profile_metadata.profile_id.clone());

    let mut last_result: Option<HardwareWalletCreationResult> = None;
    let total = account_indices.len();
    match device_kind {
        HardwareDeviceKind::Ledger => {
            let client = LedgerHardwareDerivationClient::connect().await?;
            let active = client.active_profile_session(&path).await?;
            for account_index in account_indices {
                let descriptor = HardwareDerivationDescriptor::ledger_eip1024_v1(
                    path.clone(),
                    account_index,
                    profile_fingerprint.clone(),
                    sync_intent,
                );
                active.verify_descriptor(&descriptor)?;
                let _ = send_hardware_profile_approval_progress(
                    HardwareDeviceKind::Ledger,
                    hardware_profile_approval_prompt_for_descriptor(&descriptor),
                    &progress_tx,
                );
                let output = client
                    .eip1024_shared_secret(&descriptor.path, true)
                    .await
                    .map_err(hardware_approval_error)?;
                let view_access_key =
                    hardware_view_access_key_from_hardware_output(&descriptor, &output)?;
                let entropy = synthetic_entropy_from_hardware_output(&descriptor, output)?;
                last_result = Some(store_hardware_profile_account(
                    store.as_ref(),
                    &vault_view_unlock,
                    &label_prefix,
                    total,
                    account_index,
                    descriptor,
                    &entropy,
                    &view_access_key,
                    &hardware_session,
                    &profile_metadata,
                    &pending_create_new_chain_ids,
                )?);
            }
        }
        HardwareDeviceKind::Trezor => {
            let mut client = TrezorHardwareDerivationClient::connect_with_session(
                hardware_session.trezor_session_id.clone(),
            )?;
            client.set_pin_matrix_provider(trezor_pin_matrix_provider(progress_tx.clone()));
            let info = client.device_info()?;
            let _ = send_trezor_passphrase_policy_progress(
                &progress_tx,
                info.passphrase_always_on_device,
            );
            let effective_mode =
                effective_trezor_passphrase_mode(trezor_mode, info.passphrase_always_on_device);
            profile_metadata.preferred_trezor_passphrase_mode = Some(effective_mode);
            client.set_passphrase_mode(effective_mode);
            if effective_mode == TrezorPassphraseMode::EnterInApp
                && let Some(passphrase) = trezor_app_passphrase
            {
                client.set_app_passphrase_zeroizing(passphrase);
            }
            let active = client.active_profile_session(&path)?;
            hardware_session
                .trezor_session_id
                .clone_from(&active.trezor_session_id);
            hardware_session.set_trezor_passphrase_mode(effective_mode);
            for account_index in account_indices {
                let descriptor = HardwareDerivationDescriptor::trezor_cipher_key_value_v1(
                    path.clone(),
                    account_index,
                    profile_fingerprint.clone(),
                    sync_intent,
                );
                active.verify_descriptor(&descriptor)?;
                let _ = send_hardware_profile_approval_progress(
                    HardwareDeviceKind::Trezor,
                    hardware_profile_approval_prompt_for_descriptor(&descriptor),
                    &progress_tx,
                );
                let output = client
                    .cipher_key_value(&descriptor)
                    .map_err(hardware_approval_error)?;
                let view_access_key =
                    hardware_view_access_key_from_hardware_output(&descriptor, &output)?;
                let entropy = synthetic_entropy_from_hardware_output(&descriptor, output)?;
                last_result = Some(store_hardware_profile_account(
                    store.as_ref(),
                    &vault_view_unlock,
                    &label_prefix,
                    total,
                    account_index,
                    descriptor,
                    &entropy,
                    &view_access_key,
                    &hardware_session,
                    &profile_metadata,
                    &pending_create_new_chain_ids,
                )?);
            }
        }
    }
    last_result.ok_or(HardwareWalletCreationError::Vault(
        VaultError::InvalidHardwareAccountRecoveryRange,
    ))
}

#[cfg(feature = "hardware")]
fn store_hardware_profile_account(
    store: &DesktopVaultStore,
    vault_view_unlock: &ViewUnlock,
    label_prefix: &str,
    total: usize,
    account_index: u32,
    descriptor: HardwareDerivationDescriptor,
    entropy: &SyntheticRailgunEntropy,
    view_access_key: &HardwareViewAccessKey,
    hardware_session: &HardwareProfileSession,
    profile_metadata: &HardwareProfileMetadata,
    pending_create_new_chain_ids: &BTreeSet<u64>,
) -> Result<HardwareWalletCreationResult, HardwareWalletCreationError> {
    let wallet_id = generate_opaque_id()?;
    let label = if total == 1 {
        format!("{label_prefix} account {account_index}")
    } else {
        format!("{label_prefix} account {account_index} recovery")
    };
    let metadata = store
        .new_hardware_wallet_metadata_with_pending_create_new_chain_ids_for_view_unlock(
            vault_view_unlock,
            &wallet_id,
            &label,
            descriptor,
            pending_create_new_chain_ids.clone(),
        )?;
    store.store_hardware_derived_wallet_from_entropy_with_metadata_for_view(
        vault_view_unlock,
        &wallet_id,
        account_index,
        entropy.expose_secret(),
        &metadata,
        view_access_key,
    )?;
    store.store_hardware_profile_metadata_with_view_unlock(vault_view_unlock, profile_metadata)?;
    let session = store.load_hardware_view_session_with_view_unlock(
        vault_view_unlock,
        hardware_session,
        &wallet_id,
        view_access_key,
    )?;
    let metadata = store.list_wallet_metadata_with_view_unlock(vault_view_unlock, true)?;
    Ok((session, metadata))
}

#[cfg(feature = "hardware")]
pub(super) async fn create_hardware_derived_wallet(
    store: Arc<DesktopVaultStore>,
    password: Zeroizing<String>,
    wallet_id: String,
    label: String,
    device_kind: HardwareDeviceKind,
    sync_intent: HardwareWalletSyncIntent,
    explicit_account_index: Option<u32>,
    pending_create_new_chain_ids: BTreeSet<u64>,
) -> Result<HardwareWalletCreationResult, HardwareWalletCreationError> {
    let path = parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH)?;
    let (descriptor, entropy, view_access_key) = match device_kind {
        HardwareDeviceKind::Ledger => {
            let client = LedgerHardwareDerivationClient::connect().await?;
            let profile_fingerprint = client.profile_fingerprint(&path).await?;
            let account_index = match explicit_account_index {
                Some(index) => index,
                None => next_hardware_account_index(
                    store.as_ref(),
                    password.as_str(),
                    device_kind,
                    &profile_fingerprint,
                )?,
            };
            let descriptor = HardwareDerivationDescriptor::ledger_eip1024_v1(
                path,
                account_index,
                profile_fingerprint,
                sync_intent,
            );
            let output = client.eip1024_shared_secret(&descriptor.path, true).await?;
            let view_access_key =
                hardware_view_access_key_from_hardware_output(&descriptor, &output)?;
            let entropy = synthetic_entropy_from_hardware_output(&descriptor, output)?;
            (descriptor, entropy, view_access_key)
        }
        HardwareDeviceKind::Trezor => {
            let mut client = TrezorHardwareDerivationClient::connect()?;
            let profile_fingerprint = client.profile_fingerprint(&path)?;
            let account_index = match explicit_account_index {
                Some(index) => index,
                None => next_hardware_account_index(
                    store.as_ref(),
                    password.as_str(),
                    device_kind,
                    &profile_fingerprint,
                )?,
            };
            let descriptor = HardwareDerivationDescriptor::trezor_cipher_key_value_v1(
                path,
                account_index,
                profile_fingerprint,
                sync_intent,
            );
            let output = client.cipher_key_value(&descriptor)?;
            let view_access_key =
                hardware_view_access_key_from_hardware_output(&descriptor, &output)?;
            let entropy = synthetic_entropy_from_hardware_output(&descriptor, output)?;
            (descriptor, entropy, view_access_key)
        }
    };
    let (session, metadata) = store_hardware_wallet(
        store.as_ref(),
        password.as_str(),
        &wallet_id,
        &label,
        descriptor,
        &entropy,
        &view_access_key,
        pending_create_new_chain_ids,
    )?;
    Ok((session, metadata))
}

#[cfg(feature = "hardware")]
fn next_hardware_account_index(
    store: &DesktopVaultStore,
    password: &str,
    device_kind: HardwareDeviceKind,
    profile_fingerprint: &str,
) -> Result<u32, VaultError> {
    let profile = HardwareWalletProfile {
        device_kind,
        profile_fingerprint: profile_fingerprint.to_owned(),
    };
    store.next_hardware_account_index_for_profile(password, &profile)
}

#[cfg(feature = "hardware")]
fn store_hardware_wallet(
    store: &DesktopVaultStore,
    password: &str,
    wallet_id: &str,
    label: &str,
    descriptor: HardwareDerivationDescriptor,
    entropy: &SyntheticRailgunEntropy,
    view_access_key: &HardwareViewAccessKey,
    pending_create_new_chain_ids: BTreeSet<u64>,
) -> Result<(DesktopViewSession, Vec<WalletMetadataBundle>), HardwareWalletCreationError> {
    let account_index = descriptor.account_index;
    let device_kind = descriptor.device_kind;
    let profile_fingerprint = descriptor.profile_fingerprint.clone();
    let metadata = store.new_hardware_wallet_metadata_with_pending_create_new_chain_ids(
        password,
        wallet_id,
        label,
        descriptor,
        pending_create_new_chain_ids,
    )?;
    store.store_hardware_derived_wallet_from_entropy_with_metadata(
        password,
        wallet_id,
        account_index,
        entropy.expose_secret(),
        &metadata,
        view_access_key,
    )?;
    let metadata = store.list_wallet_metadata(password)?;
    let hardware_session = store.hardware_profile_session_for_fingerprint(
        password,
        device_kind,
        &profile_fingerprint,
        None,
    )?;
    let session = store.load_hardware_view_session(
        password,
        &hardware_session,
        wallet_id,
        view_access_key,
    )?;
    Ok((session, metadata))
}
