#[cfg(feature = "hardware")]
use crate::root::{dialog_max_height, secondary_dialog_content_width, ui_helpers::dialog_footer};
#[cfg(feature = "hardware")]
use gpui::{ParentElement, Styled, div, px, rgb};
#[cfg(feature = "hardware")]
use gpui_component::{Sizable, WindowExt, alert::Alert};
#[cfg(feature = "hardware")]
use ui::{
    controls::{app_input, app_muted_text, app_strong_text},
    theme,
};

#[cfg(feature = "hardware")]
use super::super::visible_wallet_metadata;
#[cfg(not(feature = "hardware"))]
use super::WalletRoot;
#[cfg(feature = "hardware")]
use super::{
    Arc, Context, DesktopVaultStore, Focusable, HardwareAccountPickerRow,
    HardwareDerivationDescriptor, HardwareDerivationMethod, HardwareDeviceKind,
    HardwareProfileApprovalPrompt, HardwareProfileMetadata, HardwareProfilePickerView,
    HardwareProfileSession, HardwareProfileStep, HardwareProfileStepStatus,
    HardwareProfileUnlockPurpose, HardwareProfileUnlockState, HardwareRailgunAccountMetadata,
    HardwareWalletCreationError, HardwareWalletSyncIntent, TrezorPassphraseMode, ViewUnlock,
    WalletMetadataBundle, WalletRoot, Window, create_hardware_profile_accounts, mpsc,
    open_hardware_account, parse_hardware_exact_recovery_index, parse_hardware_recovery_range,
    trezor_cipher_key_label, unlock_hardware_profile, vault_error_message,
    wallet_options_from_metadata,
};

#[cfg(feature = "hardware")]
pub(in crate::root) fn hardware_profile_approval_prompt_for_account(
    account: &HardwareRailgunAccountMetadata,
) -> Option<HardwareProfileApprovalPrompt> {
    hardware_profile_approval_prompt_for_descriptor(&account.descriptor)
}

#[cfg(feature = "hardware")]
pub(super) fn hardware_profile_approval_prompt_for_descriptor(
    descriptor: &HardwareDerivationDescriptor,
) -> Option<HardwareProfileApprovalPrompt> {
    match descriptor.method {
        HardwareDerivationMethod::LedgerEip1024V1 => None,
        HardwareDerivationMethod::TrezorCipherKeyValueV1 => {
            Some(HardwareProfileApprovalPrompt::TrezorCipherKeyValue(
                Arc::from(trezor_cipher_key_label(descriptor.account_index)),
            ))
        }
    }
}

#[cfg(feature = "hardware")]
pub(super) fn hardware_profile_approval_prompt_for_account_index(
    device_kind: HardwareDeviceKind,
    account_index: u32,
) -> Option<HardwareProfileApprovalPrompt> {
    match device_kind {
        HardwareDeviceKind::Ledger => None,
        HardwareDeviceKind::Trezor => Some(HardwareProfileApprovalPrompt::TrezorCipherKeyValue(
            Arc::from(trezor_cipher_key_label(account_index)),
        )),
    }
}

#[cfg(feature = "hardware")]
pub(super) const fn default_hardware_profile_label(
    device_kind: HardwareDeviceKind,
) -> &'static str {
    match device_kind {
        HardwareDeviceKind::Ledger => "Ledger hardware profile",
        HardwareDeviceKind::Trezor => "Trezor hardware profile",
    }
}

#[cfg(feature = "hardware")]
pub(in crate::root) fn hardware_account_picker_rows(
    metadata: &[WalletMetadataBundle],
    profile_id: &str,
    active_wallet_id: Option<&str>,
    purpose: HardwareProfileUnlockPurpose,
    target_wallet_id: Option<&str>,
) -> (Vec<HardwareAccountPickerRow>, Vec<HardwareAccountPickerRow>) {
    let mut matching = Vec::new();
    let mut locked = Vec::new();
    for wallet in metadata.iter().filter(|wallet| {
        wallet.status == wallet_ops::vault::WalletStatus::Active
            || (purpose == HardwareProfileUnlockPurpose::Delete
                && target_wallet_id == Some(wallet.wallet_uuid.as_str()))
    }) {
        let Some(account) = wallet.hardware_account.clone() else {
            continue;
        };
        let row = HardwareAccountPickerRow {
            wallet_id: Arc::from(wallet.wallet_uuid.clone()),
            label: Arc::from(wallet.label.clone()),
            account_index: account.account_index,
            supported: account.custody_backend.is_supported(),
            active: active_wallet_id == Some(wallet.wallet_uuid.as_str()),
            account,
        };
        if row.account.profile_id == profile_id {
            matching.push(row);
        } else {
            locked.push(row);
        }
    }
    matching.sort_by(|left, right| {
        left.account_index
            .cmp(&right.account_index)
            .then_with(|| left.label.cmp(&right.label))
    });
    locked.sort_by(|left, right| {
        left.account
            .profile_id
            .cmp(&right.account.profile_id)
            .then_with(|| left.account_index.cmp(&right.account_index))
            .then_with(|| left.label.cmp(&right.label))
    });
    (matching, locked)
}

#[cfg(feature = "hardware")]
pub(in crate::root) fn hardware_profile_auto_open_wallet_id(
    state: &HardwareProfileUnlockState,
) -> Result<Option<Arc<str>>, Arc<str>> {
    if let Some(target_wallet_id) = state.target_wallet_id.as_ref() {
        let Some(row) = state
            .accounts
            .iter()
            .find(|row| row.wallet_id.as_ref() == target_wallet_id.as_ref())
        else {
            return Err(Arc::from(
                "Connected hardware profile does not match the selected wallet. Check that the correct device and passphrase wallet are active, then try again.",
            ));
        };
        if !row.supported {
            return Err(Arc::from(
                "This hardware account custody backend is not supported by this app version.",
            ));
        }
        return Ok(Some(Arc::clone(&row.wallet_id)));
    }

    if state.purpose == HardwareProfileUnlockPurpose::Add {
        return Ok(None);
    }

    let mut supported = state.accounts.iter().filter(|row| row.supported);
    let Some(row) = supported.next() else {
        return Ok(None);
    };
    if supported.next().is_some() {
        return Ok(None);
    }
    Ok(Some(Arc::clone(&row.wallet_id)))
}

impl WalletRoot {
    #[cfg(feature = "hardware")]
    pub(in crate::root) fn begin_hardware_profile_label_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.hardware_profile_unlock.in_progress {
            return;
        }
        self.hardware_profile_unlock.error = None;
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(420.0));
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let save_root = root.clone();
            let close_root = root.clone();
            let state = root.read(cx);
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Edit hardware profile name"))
                .footer(dialog_footer("Save", true))
                .on_ok(move |_event, window, cx| {
                    save_root.update(cx, |root, cx| {
                        root.save_hardware_profile_label_edit(window, cx)
                    })
                })
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.cancel_hardware_profile_label_edit(window, cx);
                    });
                })
                .child(
                    div()
                        .w(content_width)
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            app_input(&state.hardware_profile_label_input)
                                .disabled(state.hardware_profile_unlock.in_progress),
                        )
                        .child(
                            app_muted_text(super::hardware_profile_label_warning())
                                .whitespace_normal()
                                .text_color(rgb(theme::WARNING)),
                        )
                        .children(state.hardware_profile_unlock.error.as_ref().map(|message| {
                            Alert::error("hardware-profile-label-error", message.to_string())
                                .small()
                        })),
                )
        });
        cx.defer_in(window, |root, window, cx| {
            root.hardware_profile_label_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        });
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn cancel_hardware_profile_label_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(profile) = self.hardware_profile_unlock.profile.as_ref() {
            self.hardware_profile_label_input.update(cx, |input, cx| {
                input.set_value(profile.label.clone(), window, cx);
            });
        }
        self.hardware_profile_unlock.error = None;
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn save_hardware_profile_label_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self.hardware_profile_unlock.in_progress {
            return false;
        }
        let Some(device_kind) = self.hardware_profile_unlock.device_kind else {
            self.hardware_profile_unlock.error = Some(Arc::from("Choose a hardware wallet first"));
            cx.notify();
            return false;
        };
        let Some(mut profile) = self.hardware_profile_unlock.profile.clone() else {
            self.hardware_profile_unlock.error = Some(Arc::from("Unlock a hardware profile first"));
            cx.notify();
            return false;
        };
        let label = self
            .hardware_profile_label_input
            .read(cx)
            .value()
            .trim()
            .to_owned();
        let label = if label.is_empty() {
            default_hardware_profile_label(device_kind).to_owned()
        } else {
            label
        };
        profile.label.clone_from(&label);
        self.hardware_profile_label_input.update(cx, |input, cx| {
            input.set_value(label.clone(), window, cx);
        });

        if let (Some(store), Some(vault_view_unlock)) = (
            self.vault_store.as_ref(),
            self.hardware_profile_unlock.vault_view_unlock.as_ref(),
        ) && let Err(error) =
            store.store_hardware_profile_metadata_with_view_unlock(vault_view_unlock, &profile)
        {
            self.handle_hardware_profile_vault_error(&error);
            cx.notify();
            return false;
        }

        self.hardware_profile_unlock.profile = Some(profile);
        self.active_hardware_profile = self.hardware_profile_unlock.profile.clone();
        self.hardware_profile_unlock.error = None;
        cx.notify();
        true
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn show_hardware_profile_default_sync_choice(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        self.hardware_profile_unlock.picker_view =
            HardwareProfilePickerView::ChooseDefaultSyncIntent;
        self.hardware_profile_unlock.error = None;
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn show_hardware_profile_summary(&mut self, cx: &mut Context<'_, Self>) {
        self.hardware_profile_unlock.picker_view = HardwareProfilePickerView::Summary;
        self.hardware_profile_unlock.error = None;
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn toggle_hardware_profile_advanced(&mut self, cx: &mut Context<'_, Self>) {
        self.hardware_profile_unlock.advanced_open = !self.hardware_profile_unlock.advanced_open;
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn setup_default_hardware_account_from_profile_picker(
        &mut self,
        sync_intent: HardwareWalletSyncIntent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if sync_intent == HardwareWalletSyncIntent::RecoverExisting {
            self.recover_hardware_account_zero_from_profile_picker(window, cx);
            return;
        }
        self.create_hardware_accounts_from_profile_picker(
            vec![DesktopVaultStore::default_hardware_recovery_account_index()],
            sync_intent,
            window,
            cx,
        );
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn unlock_hardware_profile_from_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.hardware_profile_unlock.in_progress {
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            self.hardware_profile_unlock.error =
                Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let Some(device_kind) = self.hardware_profile_unlock.device_kind else {
            self.hardware_profile_unlock.error = Some(Arc::from("Choose a hardware wallet first"));
            cx.notify();
            return;
        };
        let Some(vault_view_unlock) =
            self.hardware_profile_vault_view_unlock(store.as_ref(), window, cx)
        else {
            return;
        };
        let trezor_mode = self.hardware_profile_unlock.trezor_passphrase_mode;
        let trezor_app_passphrase = if device_kind == HardwareDeviceKind::Trezor
            && trezor_mode == TrezorPassphraseMode::EnterInApp
        {
            Some(Self::read_and_clear_input(
                &self.trezor_app_passphrase_input,
                window,
                cx,
            ))
        } else {
            self.trezor_app_passphrase_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            None
        };

        self.begin_hardware_profile_detection_progress();
        self.hardware_profile_unlock
            .clear_trezor_pin_matrix_prompt();
        self.hardware_profile_unlock.in_progress = true;
        self.hardware_profile_unlock.action_label = None;
        self.hardware_profile_unlock.approval_prompt = None;
        self.hardware_profile_unlock.error = None;
        let profile_generation = self.next_hardware_profile_action_generation();
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        Self::spawn_hardware_profile_progress_listener(profile_generation, progress_rx, window, cx);
        cx.notify();

        let join = self.runtime.spawn(unlock_hardware_profile(
            store,
            vault_view_unlock,
            device_kind,
            trezor_mode,
            trezor_app_passphrase,
            progress_tx,
        ));
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if !root.hardware_profile_action_is_current(profile_generation) {
                    return;
                }
                root.hardware_profile_unlock.in_progress = false;
                root.hardware_profile_unlock.action_label = None;
                match result {
                    Ok(Ok((vault_view_unlock, session, profile, metadata))) => {
                        root.install_hardware_profile_picker(
                            vault_view_unlock,
                            session,
                            profile,
                            &metadata,
                            window,
                            cx,
                        );
                    }
                    Ok(Err(HardwareWalletCreationError::Vault(error))) => {
                        root.handle_hardware_profile_vault_error(&error);
                    }
                    Ok(Err(HardwareWalletCreationError::Hardware {
                        error,
                        awaiting_approval,
                    })) => {
                        root.handle_hardware_profile_hardware_error(
                            "Hardware profile unlock failed",
                            &error,
                            awaiting_approval,
                            window,
                            cx,
                        );
                    }
                    Err(error) => {
                        tracing::warn!(%error, "hardware profile unlock task failed");
                        root.hardware_profile_unlock.error = Some(Arc::from(
                            "Hardware profile unlock failed. See logs for non-sensitive diagnostics.",
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    #[cfg(feature = "hardware")]
    fn install_hardware_profile_picker(
        &mut self,
        vault_view_unlock: Arc<ViewUnlock>,
        session: HardwareProfileSession,
        profile: HardwareProfileMetadata,
        metadata: &[WalletMetadataBundle],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.wallet_metadata =
            visible_wallet_metadata(metadata, self.revealed_passphrase_context_id.as_deref());
        self.wallet_options = wallet_options_from_metadata(self.wallet_metadata.clone());
        self.wallet_switch_generation = self.wallet_switch_generation.wrapping_add(1);
        self.clear_hardware_profile_sensitive_inputs(window, cx);
        self.sync_wallet_select(window, cx);
        let (accounts, locked_accounts) = hardware_account_picker_rows(
            metadata,
            &profile.profile_id,
            self.selected_wallet_id.as_deref(),
            self.hardware_profile_unlock.purpose,
            self.hardware_profile_unlock.target_wallet_id.as_deref(),
        );
        self.hardware_profile_label_input.update(cx, |input, cx| {
            input.set_value(profile.label.clone(), window, cx);
        });
        self.active_hardware_profile = Some(profile.clone());
        self.hardware_profile_unlock.vault_view_unlock = Some(vault_view_unlock);
        self.hardware_profile_unlock.session = Some(session);
        self.hardware_profile_unlock.profile = Some(profile);
        self.hardware_profile_unlock.accounts = accounts;
        self.hardware_profile_unlock.locked_accounts = locked_accounts;
        self.hardware_profile_unlock.picker_view = HardwareProfilePickerView::Summary;
        self.hardware_profile_unlock.advanced_open = false;
        self.hardware_profile_unlock.set_progress_step(
            HardwareProfileStep::UnlockDevice,
            HardwareProfileStepStatus::Done,
            None::<Arc<str>>,
        );
        self.hardware_profile_unlock.set_progress_step(
            HardwareProfileStep::OpenEthereumApp,
            HardwareProfileStepStatus::Done,
            None::<Arc<str>>,
        );
        self.hardware_profile_unlock.set_progress_step(
            HardwareProfileStep::ApproveRailgunRequest,
            HardwareProfileStepStatus::NotStarted,
            None::<Arc<str>>,
        );
        self.hardware_profile_unlock.error = None;
        let auto_open_wallet_id =
            match hardware_profile_auto_open_wallet_id(&self.hardware_profile_unlock) {
                Ok(wallet_id) => wallet_id,
                Err(message) => {
                    self.hardware_profile_unlock.error = Some(message);
                    None
                }
            };
        cx.notify();
        if let Some(wallet_id) = auto_open_wallet_id {
            self.open_hardware_account_from_profile_picker(wallet_id.as_ref(), window, cx);
        }
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn open_hardware_account_from_profile_picker(
        &mut self,
        wallet_id: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.hardware_profile_unlock.in_progress {
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            self.hardware_profile_unlock.error =
                Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let Some(session) = self.hardware_profile_unlock.session.clone() else {
            self.hardware_profile_unlock.error =
                Some(Arc::from("Unlock the hardware profile first"));
            cx.notify();
            return;
        };
        let Some(vault_view_unlock) = self.hardware_profile_unlock.vault_view_unlock.clone() else {
            self.hardware_profile_unlock.error =
                Some(Arc::from("Unlock the hardware profile first"));
            cx.notify();
            return;
        };
        let Some(row) = self
            .hardware_profile_unlock
            .accounts
            .iter()
            .find(|row| row.wallet_id.as_ref() == wallet_id)
            .cloned()
        else {
            self.hardware_profile_unlock.error = Some(Arc::from(
                "Selected account is not in the unlocked hardware profile",
            ));
            cx.notify();
            return;
        };
        if !row.supported {
            self.hardware_profile_unlock.error = Some(Arc::from(
                "This hardware account custody backend is not supported by this app version.",
            ));
            cx.notify();
            return;
        }
        let trezor_mode = self.hardware_profile_unlock.trezor_passphrase_mode;
        let trezor_app_passphrase = self.read_trezor_app_passphrase_for_profile_operation(
            row.account.descriptor.device_kind,
            trezor_mode,
            window,
            cx,
        );
        self.hardware_profile_unlock.in_progress = true;
        self.hardware_profile_unlock.action_label = None;
        self.hardware_profile_unlock.approval_prompt =
            hardware_profile_approval_prompt_for_account(&row.account);
        self.hardware_profile_unlock.error = None;
        let profile_generation = self.next_hardware_profile_action_generation();
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        Self::spawn_hardware_profile_progress_listener(profile_generation, progress_rx, window, cx);
        cx.notify();

        let join = self.runtime.spawn(open_hardware_account(
            store,
            vault_view_unlock,
            session,
            wallet_id.to_owned(),
            row.account,
            trezor_mode,
            trezor_app_passphrase,
            progress_tx,
        ));
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if !root.hardware_profile_action_is_current(profile_generation) {
                    return;
                }
                root.hardware_profile_unlock.in_progress = false;
                root.hardware_profile_unlock.action_label = None;
                match result {
                    Ok(Ok((session, metadata))) => {
                        root.manage_wallets
                            .mark_hardware_delete_target_opened(session.wallet_id());
                        root.install_view_session(session, &metadata, window, cx);
                    }
                    Ok(Err(HardwareWalletCreationError::Vault(error))) => {
                        root.handle_hardware_profile_vault_error(&error);
                    }
                    Ok(Err(HardwareWalletCreationError::Hardware {
                        error,
                        awaiting_approval,
                    })) => {
                        root.handle_hardware_profile_hardware_error(
                            "Hardware account unlock failed",
                            &error,
                            awaiting_approval,
                            window,
                            cx,
                        );
                    }
                    Err(error) => {
                        tracing::warn!(%error, "hardware account unlock task failed");
                        root.hardware_profile_unlock.error = Some(Arc::from(
                            "Hardware account unlock failed. See logs for non-sensitive diagnostics.",
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn add_hardware_subaccount_from_profile_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.clone() else {
            self.hardware_profile_unlock.error =
                Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let Some(vault_view_unlock) = self.hardware_profile_unlock.vault_view_unlock.clone() else {
            self.hardware_profile_unlock.error =
                Some(Arc::from("Unlock the hardware profile first"));
            cx.notify();
            return;
        };
        let Some(profile) = self
            .hardware_profile_unlock
            .session
            .as_ref()
            .and_then(HardwareProfileSession::wallet_profile)
        else {
            self.hardware_profile_unlock.error = Some(Arc::from(
                "This hardware profile is not stored yet. Create or recover account 0 first.",
            ));
            cx.notify();
            return;
        };
        match store
            .next_hardware_account_index_for_profile_with_view_unlock(&vault_view_unlock, &profile)
        {
            Ok(account_index) => self.create_hardware_accounts_from_profile_picker(
                vec![account_index],
                HardwareWalletSyncIntent::CreateNew,
                window,
                cx,
            ),
            Err(error) => {
                self.hardware_profile_unlock.error = Some(vault_error_message(&error));
                cx.notify();
            }
        }
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn recover_hardware_account_zero_from_profile_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.create_hardware_accounts_from_profile_picker(
            vec![DesktopVaultStore::default_hardware_recovery_account_index()],
            HardwareWalletSyncIntent::RecoverExisting,
            window,
            cx,
        );
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn recover_hardware_exact_account_from_profile_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let value = self
            .hardware_profile_exact_index_input
            .read(cx)
            .value()
            .to_string();
        match parse_hardware_exact_recovery_index(&value) {
            Ok(account_index) => self.create_hardware_accounts_from_profile_picker(
                vec![account_index],
                HardwareWalletSyncIntent::RecoverExisting,
                window,
                cx,
            ),
            Err(message) => {
                self.hardware_profile_unlock.error = Some(Arc::from(message));
                cx.notify();
            }
        }
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn recover_hardware_range_from_profile_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let start = self
            .hardware_profile_recovery_start_input
            .read(cx)
            .value()
            .to_string();
        let count = self
            .hardware_profile_recovery_count_input
            .read(cx)
            .value()
            .to_string();
        match parse_hardware_recovery_range(&start, &count) {
            Ok(indices) => self.create_hardware_accounts_from_profile_picker(
                indices,
                HardwareWalletSyncIntent::RecoverExisting,
                window,
                cx,
            ),
            Err(message) => {
                self.hardware_profile_unlock.error = Some(Arc::from(message));
                cx.notify();
            }
        }
    }

    #[cfg(feature = "hardware")]
    fn create_hardware_accounts_from_profile_picker(
        &mut self,
        account_indices: Vec<u32>,
        sync_intent: HardwareWalletSyncIntent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.hardware_profile_unlock.in_progress {
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            self.hardware_profile_unlock.error =
                Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let Some(device_kind) = self.hardware_profile_unlock.device_kind else {
            self.hardware_profile_unlock.error = Some(Arc::from("Unlock a hardware profile first"));
            cx.notify();
            return;
        };
        let Some(session) = self.hardware_profile_unlock.session.clone() else {
            self.hardware_profile_unlock.error = Some(Arc::from("Unlock a hardware profile first"));
            cx.notify();
            return;
        };
        let Some(vault_view_unlock) = self.hardware_profile_unlock.vault_view_unlock.clone() else {
            self.hardware_profile_unlock.error =
                Some(Arc::from("Unlock the hardware profile first"));
            cx.notify();
            return;
        };
        if account_indices.is_empty() {
            self.hardware_profile_unlock.error = Some(Arc::from(
                "Enter at least one hardware account index to recover",
            ));
            cx.notify();
            return;
        }
        let label_prefix = self
            .hardware_profile_label_input
            .read(cx)
            .value()
            .to_string();
        let label_prefix = if label_prefix.trim().is_empty() {
            default_hardware_profile_label(device_kind).to_owned()
        } else {
            label_prefix.trim().to_owned()
        };
        let trezor_mode = self.hardware_profile_unlock.trezor_passphrase_mode;
        let trezor_app_passphrase = self.read_trezor_app_passphrase_for_profile_operation(
            device_kind,
            trezor_mode,
            window,
            cx,
        );
        self.hardware_profile_unlock.in_progress = true;
        self.hardware_profile_unlock.action_label = None;
        self.hardware_profile_unlock.approval_prompt =
            account_indices.first().and_then(|account_index| {
                hardware_profile_approval_prompt_for_account_index(device_kind, *account_index)
            });
        self.hardware_profile_unlock.error = None;
        let profile_generation = self.next_hardware_profile_action_generation();
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        let pending_create_new_chain_ids = self.enabled_chain_ids_for_created_wallet();
        Self::spawn_hardware_profile_progress_listener(profile_generation, progress_rx, window, cx);
        cx.notify();

        let join = self.runtime.spawn(create_hardware_profile_accounts(
            store,
            vault_view_unlock,
            label_prefix,
            device_kind,
            session,
            account_indices,
            sync_intent,
            pending_create_new_chain_ids,
            trezor_mode,
            trezor_app_passphrase,
            progress_tx,
        ));
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if !root.hardware_profile_action_is_current(profile_generation) {
                    return;
                }
                root.hardware_profile_unlock.in_progress = false;
                root.hardware_profile_unlock.action_label = None;
                match result {
                    Ok(Ok((session, metadata))) => {
                        if sync_intent == HardwareWalletSyncIntent::CreateNew {
                            root.enter_new_wallet_view_unlocked(session, &metadata, window, cx);
                        } else {
                            root.install_view_session(session, &metadata, window, cx);
                        }
                    }
                    Ok(Err(HardwareWalletCreationError::Vault(error))) => {
                        root.handle_hardware_profile_vault_error(&error);
                    }
                    Ok(Err(HardwareWalletCreationError::Hardware {
                        error,
                        awaiting_approval,
                    })) => {
                        root.handle_hardware_profile_hardware_error(
                            "Hardware account creation failed",
                            &error,
                            awaiting_approval,
                            window,
                            cx,
                        );
                    }
                    Err(error) => {
                        tracing::warn!(%error, "hardware profile account creation task failed");
                        root.hardware_profile_unlock.error = Some(Arc::from(
                            "Hardware account creation failed. See logs for non-sensitive diagnostics.",
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
