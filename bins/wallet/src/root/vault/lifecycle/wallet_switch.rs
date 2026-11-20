#[allow(clippy::wildcard_imports)]
use super::*;

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

    pub(in crate::root::vault) fn open_add_wallet_dialog_with_mode(
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
        let content_width = secondary_dialog_content_width(dialog_width);
        let lease = Rc::new(Cell::new(true));
        self.add_wallet_dialog_lease = Rc::downgrade(&lease);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            // Builder ownership also invalidates programmatically closed dialogs.
            let identity = Rc::downgrade(&lease);
            let content_root = root.clone();
            let close_root = root.clone();
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(dialog_max_height)
                .title(app_strong_text("Add wallet"))
                .on_close(move |_, window, cx| {
                    if let Some(lease) = identity.upgrade() {
                        lease.set(false);
                    }
                    close_root.update(cx, |root, cx| {
                        root.vault_inputs
                            .import_mnemonic_input
                            .update(cx, |input, cx| input.set_value("", window, cx));
                    });
                })
                .child(
                    content_root
                        .read(cx)
                        .render_add_wallet_dialog_content(content_root.clone(), content_width),
                )
        });
        cx.defer_in(window, move |root, window, cx| {
            root.set_wallet_name_input(&label, window, cx);
            root.clear_add_wallet_password(window, cx);
            root.vault_inputs
                .import_mnemonic_input
                .update(cx, |input, cx| input.set_value("", window, cx));
        });
    }

    #[cfg_attr(not(feature = "hardware"), allow(clippy::needless_pass_by_ref_mut))]
    pub(in crate::root::vault) fn switch_active_wallet(
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
            self.wallet_switch_generation = self.wallet_switch_generation.wrapping_add(1);
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
        self.wallet_switch_loading_generation = Some(switch_generation);
        self.vault_error = None;
        let wallet_id_string = wallet_id.to_owned();
        let metadata = self.wallet_metadata.clone();
        let join = self.runtime.spawn_blocking(move || {
            store.load_view_session_with_view_session(current_session.as_ref(), &wallet_id_string)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.wallet_switch_loading_generation == Some(switch_generation) {
                    root.wallet_switch_loading_generation = None;
                }
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
                        tracing::warn!(target: LOG_TARGET, %error, "wallet view-session task failed");
                        root.set_vault_error(
                            "Failed to open wallet. See logs for non-sensitive diagnostics.",
                            cx,
                        );
                        root.sync_wallet_select(window, cx);
                    }
                }
                if root.vault_error.is_some() {
                    root.gateway.private_selection_message =
                        Some("Could not switch wallets. Open the desktop app to continue.");
                }
                root.publish_gateway_desktop_state();
            });
        })
        .detach();
        self.publish_gateway_desktop_state();
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

        let gateway_unlock = self.gateway_unlock_continuation();
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
                    Ok(Ok(session)) => root.install_view_session_for_gateway_unlock(
                        session,
                        &metadata,
                        gateway_unlock,
                        window,
                        cx,
                    ),
                    Ok(Err(error)) => {
                        root.handle_vault_error(&error, cx);
                        root.sync_wallet_select(window, cx);
                    }
                    Err(error) => {
                        tracing::warn!(target: LOG_TARGET, %error, "wallet view-session task failed");
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
    pub(in crate::root::vault) fn deactivate_wallet_and_switch(
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
}
