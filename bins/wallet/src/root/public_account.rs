use std::sync::Arc;

use alloy::primitives::Address;
#[cfg(feature = "hardware")]
use gpui::rgb;
use gpui::{
    Context, Entity, Focusable, IntoElement, ParentElement, Pixels, SharedString, Styled, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Disableable, Sizable, WindowExt,
    alert::Alert,
    button::ButtonVariants,
    checkbox::Checkbox,
    menu::{DropdownMenu, PopupMenuItem},
};
use railgun_ui::{chain_name, short_address};
use ui::controls::{app_button, app_input, app_masked_input, app_muted_text, app_strong_text};
#[cfg(feature = "hardware")]
use ui::theme;
use wallet_ops::{
    PublicAssetId, PublicBalanceEntry,
    hardware::{HardwareDeviceKind, HardwarePublicAccountDescriptor},
    vault::{
        DesktopVaultStore, DesktopViewSession, PublicAccountMetadata, PublicAccountStatus,
        WalletSource, public_account_default_label,
    },
};
use zeroize::Zeroizing;

mod assets;
mod commands;
mod components;
mod hardware;
mod identicon;
pub(super) mod list;
mod qr;
mod types;

pub(super) use components::{
    next_public_account_label_number, public_account_display_label, public_account_matches_search,
    public_account_source_label,
};
#[cfg(feature = "hardware")]
use hardware::{HardwarePublicAccountDerivationProgress, create_hardware_public_account};
pub(super) use hardware::{
    HardwarePublicAccountDerivationStatus, hardware_public_account_setup_copy,
};
use hardware::{
    hardware_public_device_label, render_hardware_public_account_checking,
    render_hardware_public_account_confirmation_wait,
};
#[cfg(test)]
pub(super) use identicon::{
    PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT, PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE,
    public_account_identicon_color, public_account_identicon_pattern,
};
pub(super) use qr::{public_address_qr_payload, render_public_address_qr_dialog_content};
pub(super) use types::PublicAccountFormState;
#[cfg(test)]
pub(super) use ui::public_address::{
    PUBLIC_ADDRESS_QR_QUIET_ZONE_MODULES, public_address_qr_module_range,
};

use super::dialogs::PublicAccountDialogKind;
use super::participant::{remove_global_participant, remove_scoped_participant};
use super::public_action::{PublicActionMode, PublicSendKind};
use super::{
    ConfirmationDialogProps, PUBLIC_ACCOUNT_DIALOG_WIDTH, PUBLIC_ADDRESS_QR_DIALOG_WIDTH,
    WalletRoot, confirmation_dialog, dialog_max_height, public_account_visible_balances_for_chain,
    secondary_dialog_content_width, vault_error_kind,
};

pub(super) fn restored_public_account_selection(
    accounts: &[PublicAccountMetadata],
    current: Option<&str>,
    ui_state: &wallet_ops::settings::WalletUiState,
    wallet_id: &str,
) -> Option<Arc<str>> {
    // Preserve an explicit desktop selection, including an inactive account being inspected.
    current
        .filter(|uuid| {
            accounts.iter().any(|account| {
                account.public_account_uuid == *uuid && account.is_scoped_to_wallet(wallet_id)
            })
        })
        .or_else(|| {
            let remembered = ui_state.last_public_accounts.get(wallet_id)?;
            accounts
                .iter()
                .find(|account| {
                    account.public_account_uuid == *remembered
                        && account.is_active_for_wallet(wallet_id)
                })
                .map(|account| account.public_account_uuid.as_str())
        })
        .or_else(|| {
            accounts
                .iter()
                .find(|account| account.is_active_for_wallet(wallet_id))
                .map(|account| account.public_account_uuid.as_str())
        })
        .map(Arc::from)
}

impl WalletRoot {
    pub(super) fn executor_owner_for_public_chain(
        &self,
        chain_id: u64,
    ) -> Option<Arc<wallet_ops::ExecutorOwner>> {
        match self.chain_states.get(&chain_id) {
            Some(
                super::ChainUtxoState::Ready { session, .. }
                | super::ChainUtxoState::Syncing { session, .. },
            ) => session.executor_owner(),
            _ => None,
        }
    }

    pub(super) fn open_public_account_dialog(
        &mut self,
        kind: PublicAccountDialogKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.public_form.error = None;
        self.clear_public_account_dialog_inputs(kind, window, cx);
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(PUBLIC_ACCOUNT_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(dialog_max_height)
                .title(app_strong_text(kind.title()))
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.public_form.error = None;
                        root.clear_public_account_dialog_inputs(kind, window, cx);
                    });
                })
                .child(content_root.read(cx).render_public_account_dialog_content(
                    content_root.clone(),
                    kind,
                    content_width,
                ))
        });
        cx.defer_in(window, move |root, window, cx| {
            root.focus_public_account_dialog_input(kind, window, cx);
        });
    }

    pub(super) fn open_public_account_edit_dialog(
        &mut self,
        public_account_uuid: Arc<str>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.public_form.error = None;
        self.public_form.editing_account_uuid = Some(public_account_uuid);
        self.sync_public_edit_label_input(window, cx);
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(PUBLIC_ACCOUNT_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(dialog_max_height)
                .title(app_strong_text(PublicAccountDialogKind::EditLabel.title()))
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.public_form.error = None;
                        root.clear_public_account_dialog_inputs(
                            PublicAccountDialogKind::EditLabel,
                            window,
                            cx,
                        );
                    });
                })
                .child(content_root.read(cx).render_public_account_dialog_content(
                    content_root.clone(),
                    PublicAccountDialogKind::EditLabel,
                    content_width,
                ))
        });
        cx.defer_in(window, |root, window, cx| {
            root.focus_public_account_dialog_input(PublicAccountDialogKind::EditLabel, window, cx);
        });
    }

    pub(super) fn open_public_address_qr_dialog(
        &self,
        public_account_uuid: &str,
        label: Option<String>,
        address: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        let dialog_width =
            (window.viewport_size().width * 0.92).min(PUBLIC_ADDRESS_QR_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let address_text = SharedString::from(public_address_qr_payload(address));
        let account_label = label.map(SharedString::from);
        let chain_label = chain_name(self.selected_chain)
            .map_or_else(|| format!("chain {}", self.selected_chain), str::to_owned);
        let copy_id = SharedString::from(format!(
            "wallet-public-address-qr-copy-{public_account_uuid}"
        ));
        let receive_warning = SharedString::from(format!(
            "Send only public {chain_label} assets to this address."
        ));
        let root = cx.entity().downgrade();
        let generation = self.active_wallet_generation;
        window.open_dialog(cx, move |dialog, _window, _cx| {
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Public account address"))
                .child(render_public_address_qr_dialog_content(
                    account_label.clone(),
                    address_text.clone(),
                    Some(receive_warning.clone()),
                    copy_id.clone(),
                    content_width,
                    {
                        let root = root.clone();
                        let address = address_text.clone();
                        move |window, cx| {
                            if root.upgrade().is_some_and(|root| {
                                let root = root.read(cx);
                                root.active_wallet_generation == generation
                                    && root.view_session.is_some()
                            }) {
                                ui::clipboard::copy_to_clipboard_with_toast(
                                    address.clone(),
                                    window,
                                    cx,
                                );
                            }
                        }
                    },
                ))
        });
    }

    fn focus_public_account_dialog_input(
        &self,
        kind: PublicAccountDialogKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            PublicAccountDialogKind::Derive => self
                .public_form
                .add_password_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx),
            PublicAccountDialogKind::Import => self
                .public_form
                .import_private_key_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx),
            PublicAccountDialogKind::EditLabel => self
                .public_form
                .edit_label_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx),
        }
    }

    pub(super) fn render_public_wallet_body(
        &self,
        root: &Entity<Self>,
        window: &Window,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        if let Some(view) = self.stealth_accounts_body() {
            return view.into_any_element();
        }
        let refresh_root = root.clone();

        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .w(list::dimension(list::CONTENT_WIDTH))
                    .max_w_full()
                    .h_full()
                    .min_h(px(0.0))
                    .mx_auto()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(self.render_public_list_controls(root))
                            .child(div().flex_1().min_w(px(0.0)))
                            .child(self.render_walletconnect_toolbar_button(root))
                            .child(
                                app_button(
                                    "wallet-public-refresh",
                                    if self.public_balance_refreshing {
                                        "Refreshing…"
                                    } else {
                                        "Refresh"
                                    },
                                )
                                .outline()
                                .small()
                                .icon(
                                    gpui_component::Icon::empty()
                                        .path(ui::icons::refresh_ccw_icon_path()),
                                )
                                .loading(self.public_balance_refreshing)
                                .disabled(
                                    self.public_balance_refreshing
                                        || !self.has_active_public_accounts(),
                                )
                                .on_click(
                                    move |_event, _window, cx| {
                                        refresh_root.update(cx, |root, cx| {
                                            root.schedule_public_balance_refresh(cx);
                                        });
                                    },
                                ),
                            )
                            .child(self.render_public_add_account_dropdown(root)),
                    )
                    .child(self.render_public_accounts_summary(root, cx))
                    .children(self.public_form.error.as_ref().map(|message| {
                        div()
                            .flex_none()
                            .child(Alert::error("wallet-public-error", message.to_string()).small())
                    }))
                    .child(self.render_public_account_list(root, window, cx)),
            )
            .into_any_element()
    }

    pub(super) fn clear_public_wallet_runtime_state(&mut self, cx: &mut Context<'_, Self>) {
        self.clear_stealth_accounts(cx);
        self.public_balance_cache.clear();
        self.public_accounts.clear();
        self.public_balance_snapshot = None;
        self.public_balance_error = None;
        self.public_balance_refreshing = false;
        self.public_inactive_balance_error = None;
        self.public_inactive_balance_refreshing = false;
        self.public_form.selected_account_uuid = None;
        self.public_form.editing_account_uuid = None;
        self.public_form.selected_asset = None;
        self.public_form.mimic_railway_shield = self.mimic_railway_shields_by_default;
        self.public_form.public_send_kind = PublicSendKind::Transfer;
        self.public_form.advanced_send_estimate = None;
        self.public_form.advanced_send_estimate_invalidated = false;
        self.invalidate_advanced_public_send_estimate();
        self.clear_public_action_progress_state();
        self.public_form.next_derived_index = None;
        self.public_form.next_account_label_number = 1;
        self.public_form.error = None;
        self.public_form.send_error = None;
        self.public_form.shield_error = None;
        self.walletconnect.clear_runtime();
        self.sync_walletconnect_attention();
        self.public_form.adding_account = false;
        self.public_form.hardware_derivation_status = HardwarePublicAccountDerivationStatus::Idle;
        self.public_form.hardware_confirmation_address = None;
        self.public_form.importing_account = false;
        self.public_form.sending = false;
        self.public_form.shielding = false;
        self.reset_public_asset_focus();
        self.public_form.open_section = PublicAccountStatus::Active;
    }

    pub(super) fn reset_public_wallet_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.clear_public_wallet_runtime_state(cx);
        for input in [
            &self.public_form.add_label_input,
            &self.public_form.add_password_input,
            &self.public_form.import_label_input,
            &self.public_form.import_private_key_input,
            &self.public_form.import_password_input,
            &self.public_form.edit_label_input,
            &self.public_form.send_recipient_input,
            &self.public_form.send_amount_input,
            &self.public_form.advanced_send_to_input,
            &self.public_form.advanced_send_value_input,
            &self.public_form.shield_amount_input,
            &self.walletconnect.uri_input,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.public_form
            .advanced_send_data_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.public_form.import_global = false;
        self.public_form.action_mode = PublicActionMode::Shield;
        self.public_form.public_send_kind = PublicSendKind::Transfer;
        self.public_form.advanced_send_estimate = None;
        self.public_form.advanced_send_estimate_invalidated = false;
        self.invalidate_advanced_public_send_estimate();
    }

    pub(super) fn clear_public_account_dialog_inputs(
        &mut self,
        kind: PublicAccountDialogKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let default_label =
            public_account_default_label(self.public_form.next_account_label_number);
        match kind {
            PublicAccountDialogKind::Derive => {
                self.public_form.adding_account = false;
                self.public_form.hardware_derivation_status =
                    HardwarePublicAccountDerivationStatus::Idle;
                self.public_form.hardware_confirmation_address = None;
                self.public_form
                    .add_label_input
                    .update(cx, |input, cx| input.set_value(&default_label, window, cx));
                self.public_form
                    .add_password_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.clear_trezor_app_passphrase_input(window, cx);
                self.clear_trezor_pin_matrix_prompt(cx);
            }
            PublicAccountDialogKind::Import => {
                self.public_form
                    .import_label_input
                    .update(cx, |input, cx| input.set_value(&default_label, window, cx));
                self.public_form
                    .import_private_key_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.public_form
                    .import_password_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.public_form.import_global = false;
            }
            PublicAccountDialogKind::EditLabel => {
                self.public_form.editing_account_uuid = None;
                self.public_form
                    .edit_label_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
            }
        }
    }

    pub(super) fn reload_public_accounts(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.as_ref() else {
            self.public_form.error = Some(Arc::from("Wallet vault storage is unavailable"));
            return;
        };
        let Some(view_session) = self.view_session.as_ref() else {
            self.public_accounts.clear();
            self.public_form.selected_account_uuid = None;
            self.publish_gateway_desktop_state();
            self.sync_walletconnect_account_select(window, cx);
            self.sync_self_broadcast_gas_payer_selects(window, cx);
            self.invalidate_blocked_shield_rescue_rows(cx);
            return;
        };
        match store.list_public_accounts_for_session(view_session.as_ref(), true) {
            Ok(accounts) => {
                self.public_form.next_account_label_number =
                    next_public_account_label_number(accounts.len());
                let selected = restored_public_account_selection(
                    &accounts,
                    self.public_form.selected_account_uuid.as_deref(),
                    &self.ui_state,
                    view_session.wallet_id(),
                );
                if self.public_accounts != accounts {
                    self.public_balance_cache.clear();
                    self.public_balance_snapshot = None;
                    self.public_balance_refreshing = false;
                    self.public_inactive_balance_refreshing = false;
                }
                self.public_accounts = accounts;
                self.public_form.selected_account_uuid = selected;
                if let Some(account) = self.selected_public_account() {
                    self.public_form.open_section = account.status;
                }
                self.public_form.next_derived_index = store
                    .next_derived_public_account_index_for_session(view_session.as_ref())
                    .ok();
                self.reconcile_public_account_selection();
                self.remember_public_account_selection();
                self.publish_gateway_desktop_state();
                self.sync_self_broadcast_gas_payer_selects(window, cx);
                self.sync_public_edit_label_input(window, cx);
                self.invalidate_blocked_shield_rescue_rows(cx);
                self.reload_walletconnect_sessions(cx);
                self.sync_walletconnect_account_select(window, cx);
            }
            Err(error) => {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    "load public accounts failed"
                );
                self.public_form.error = Some(Arc::from(error.to_string()));
            }
        }
    }

    pub(super) fn sync_public_edit_label_input(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let account_uuid = self
            .public_form
            .editing_account_uuid
            .as_ref()
            .or(self.public_form.selected_account_uuid.as_ref());
        let label = self
            .public_account_for_uuid(account_uuid.map(AsRef::as_ref))
            .and_then(|account| account.label.clone())
            .unwrap_or_default();
        self.public_form
            .edit_label_input
            .update(cx, |input, cx| input.set_value(&label, window, cx));
    }

    pub(super) fn selected_public_account(&self) -> Option<&PublicAccountMetadata> {
        self.public_account_for_uuid(
            self.public_form
                .selected_account_uuid
                .as_ref()
                .map(AsRef::as_ref),
        )
    }

    pub(super) fn public_account_for_uuid(
        &self,
        public_account_uuid: Option<&str>,
    ) -> Option<&PublicAccountMetadata> {
        let selected = public_account_uuid?;
        self.public_accounts.iter().find(|account| {
            account.public_account_uuid == selected
                && account.is_available_on_chain(self.selected_chain)
        })
    }

    pub(super) fn set_public_selected_balance(
        &mut self,
        public_account_uuid: Arc<str>,
        asset: PublicAssetId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.public_form.selected_account_uuid = Some(public_account_uuid);
        self.public_form.selected_asset = Some(asset);
        // Selecting a balance starts a fresh shield draft, so restore the configured default.
        self.public_form.mimic_railway_shield = self.mimic_railway_shields_by_default;
        self.public_form.public_send_kind = PublicSendKind::Transfer;
        self.public_form.advanced_send_estimate = None;
        self.public_form.advanced_send_estimate_invalidated = false;
        self.invalidate_advanced_public_send_estimate();
        self.public_form.send_error = None;
        self.public_form.shield_error = None;
        self.remember_public_account_selection();
        self.sync_public_edit_label_input(window, cx);
        self.publish_gateway_desktop_state();
        cx.notify();
    }

    fn remember_public_account_selection(&mut self) {
        let Some(view) = self.view_session.as_ref() else {
            return;
        };
        let Some(account) = self
            .selected_public_account()
            .filter(|account| account.is_active_for_wallet(view.wallet_id()))
        else {
            return;
        };
        if self.ui_state.last_public_accounts.get(view.wallet_id())
            != Some(&account.public_account_uuid)
        {
            let wallet_id = view.wallet_id().to_owned();
            let account_uuid = account.public_account_uuid.clone();
            self.ui_state
                .last_public_accounts
                .insert(wallet_id, account_uuid);
            self.save_ui_state();
        }
    }

    pub(super) fn set_public_account_section_open(
        &mut self,
        status: PublicAccountStatus,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.public_form.open_section = status;
        self.public_form
            .list_scroll
            .set_offset(gpui::Point::default());
        self.reconcile_public_account_selection();
        self.public_form.list_focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn has_active_public_accounts(&self) -> bool {
        self.public_accounts.iter().any(|account| {
            account.status == PublicAccountStatus::Active
                && account.is_available_on_chain(self.selected_chain)
        })
    }

    pub(super) fn add_public_derived_account_from_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.adding_account
            || self.public_form.hardware_derivation_status
                == HardwarePublicAccountDerivationStatus::AwaitingAddressConfirmation
        {
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            self.public_form.error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            self.public_form.error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return;
        };
        let label = self
            .public_form
            .add_label_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if label.is_empty() {
            self.public_form.error = Some(Arc::from("Enter an account label"));
            cx.notify();
            return;
        }
        if let Some(device_kind) = self.selected_hardware_public_device_kind() {
            #[cfg(feature = "hardware")]
            let trezor_app_passphrase =
                view_session.hardware_profile_session().and_then(|session| {
                    self.read_trezor_app_passphrase_for_hardware_session(session, window, cx)
                });
            #[cfg(not(feature = "hardware"))]
            let trezor_app_passphrase = None;
            self.add_hardware_public_account_from_input(
                store,
                view_session,
                device_kind,
                label,
                trezor_app_passphrase,
                window,
                cx,
            );
            return;
        }
        let password = Self::read_and_clear_input(&self.public_form.add_password_input, window, cx);
        if password.trim().is_empty() {
            self.public_form.error = Some(Arc::from("Enter the vault password to add an account"));
            cx.notify();
            return;
        }
        self.public_form.adding_account = true;
        self.public_form.error = None;
        let result = store.add_derived_public_account_with_session(
            password.as_str(),
            view_session.as_ref(),
            Some(&label),
            self.protected_software_seed_session.as_deref(),
        );
        self.public_form.adding_account = false;
        match result {
            Ok(account) => {
                self.public_form.selected_account_uuid =
                    Some(Arc::from(account.public_account_uuid.as_str()));
                self.public_form
                    .add_label_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.reload_public_accounts(window, cx);
                self.schedule_public_balance_refresh(cx);
                window.close_all_dialogs(cx);
            }
            Err(error) => {
                self.public_form.error = Some(Arc::from(error.to_string()));
            }
        }
        cx.notify();
    }

    fn add_hardware_public_account_from_input(
        &mut self,
        store: Arc<DesktopVaultStore>,
        view_session: Arc<DesktopViewSession>,
        device_kind: HardwareDeviceKind,
        label: String,
        trezor_app_passphrase: Option<Zeroizing<String>>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.public_form.adding_account = true;
        self.public_form.hardware_derivation_status =
            HardwarePublicAccountDerivationStatus::CheckingDevice;
        self.public_form.hardware_confirmation_address = None;
        self.public_form.error = None;

        #[cfg(feature = "hardware")]
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        #[cfg(feature = "hardware")]
        let trezor_pin_matrix_provider = if device_kind == HardwareDeviceKind::Trezor {
            Some(self.trezor_pin_matrix_provider_for_operation(window, cx))
        } else {
            None
        };

        #[cfg(feature = "hardware")]
        let join = self.runtime.spawn(async move {
            create_hardware_public_account(
                store,
                view_session,
                device_kind,
                label,
                trezor_app_passphrase,
                trezor_pin_matrix_provider,
                progress_tx,
            )
            .await
        });

        #[cfg(not(feature = "hardware"))]
        let join: tokio::task::JoinHandle<Result<PublicAccountMetadata, String>> =
            self.runtime.spawn(async move {
                let _ = (
                    store,
                    view_session,
                    device_kind,
                    label,
                    trezor_app_passphrase,
                );
                Err("hardware public account support is not enabled in this build".to_owned())
            });

        #[cfg(feature = "hardware")]
        cx.spawn_in(window, async move |this, cx| {
            while let Some(progress) = progress_rx.recv().await {
                let Ok(active) = this.update(cx, |root, cx| {
                    if !root.public_form.adding_account {
                        return false;
                    }
                    match progress {
                        HardwarePublicAccountDerivationProgress::CheckingDevice => {
                            root.public_form.hardware_derivation_status =
                                HardwarePublicAccountDerivationStatus::CheckingDevice;
                            root.public_form.hardware_confirmation_address = None;
                        }
                        HardwarePublicAccountDerivationProgress::AwaitingAddressConfirmation(
                            address,
                        ) => {
                            root.public_form.hardware_derivation_status =
                                HardwarePublicAccountDerivationStatus::AwaitingAddressConfirmation;
                            root.public_form.hardware_confirmation_address = Some(address);
                        }
                    }
                    cx.notify();
                    true
                }) else {
                    break;
                };
                if !active {
                    break;
                }
            }
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.public_form.adding_account = false;
                root.public_form.hardware_derivation_status =
                    HardwarePublicAccountDerivationStatus::Idle;
                root.public_form.hardware_confirmation_address = None;
                match result {
                    #[cfg(feature = "hardware")]
                    Ok(Ok((account, hardware_session))) => {
                        root.refresh_active_hardware_profile_session(hardware_session, cx);
                        root.public_form.selected_account_uuid =
                            Some(Arc::from(account.public_account_uuid.as_str()));
                        root.public_form
                            .add_label_input
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        root.reload_public_accounts(window, cx);
                        root.schedule_public_balance_refresh(cx);
                        root.clear_trezor_pin_matrix_prompt(cx);
                        window.close_all_dialogs(cx);
                    }
                    #[cfg(not(feature = "hardware"))]
                    Ok(Ok(account)) => {
                        root.public_form.selected_account_uuid =
                            Some(Arc::from(account.public_account_uuid.as_str()));
                        root.public_form
                            .add_label_input
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        root.reload_public_accounts(window, cx);
                        root.schedule_public_balance_refresh(cx);
                        window.close_all_dialogs(cx);
                    }
                    Ok(Err(error)) => {
                        root.discard_active_trezor_session_if_stale(&error, cx);
                        root.public_form.error = Some(Arc::from(error));
                    }
                    Err(error) => {
                        root.public_form.error = Some(Arc::from(error.to_string()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn selected_hardware_public_device_kind(&self) -> Option<HardwareDeviceKind> {
        match self.selected_wallet_source() {
            WalletSource::LedgerDerived => Some(HardwareDeviceKind::Ledger),
            WalletSource::TrezorDerived => Some(HardwareDeviceKind::Trezor),
            WalletSource::Generated | WalletSource::Imported => None,
        }
    }

    pub(super) fn import_public_account_from_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.importing_account {
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            self.public_form.error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            self.public_form.error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return;
        };
        let label = self
            .public_form
            .import_label_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if label.is_empty() {
            self.public_form.error = Some(Arc::from("Enter an account label"));
            cx.notify();
            return;
        }
        let private_key =
            Self::read_and_clear_input(&self.public_form.import_private_key_input, window, cx);
        let password =
            Self::read_and_clear_input(&self.public_form.import_password_input, window, cx);
        if private_key.trim().is_empty() || password.trim().is_empty() {
            self.public_form.error = Some(Arc::from(
                "Enter a private key and vault password to import an account",
            ));
            cx.notify();
            return;
        }
        let global = self.public_form.import_global;
        self.public_form.importing_account = true;
        self.public_form.error = None;
        let result = store.import_public_account(
            password.as_str(),
            view_session.as_ref(),
            private_key.as_str(),
            Some(&label),
            global,
        );
        self.public_form.importing_account = false;
        match result {
            Ok(account) => {
                self.public_form.selected_account_uuid =
                    Some(Arc::from(account.public_account_uuid.as_str()));
                self.public_form
                    .import_label_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.public_form.import_global = false;
                self.reload_public_accounts(window, cx);
                self.schedule_public_balance_refresh(cx);
                window.close_all_dialogs(cx);
            }
            Err(error) => {
                self.public_form.error = Some(Arc::from(error.to_string()));
            }
        }
        cx.notify();
    }

    pub(super) fn update_selected_public_account_label(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.clone() else {
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            return;
        };
        let Some(account_uuid) = self
            .public_form
            .editing_account_uuid
            .clone()
            .or_else(|| self.public_form.selected_account_uuid.clone())
        else {
            self.public_form.error = Some(Arc::from("Select a public account first"));
            cx.notify();
            return;
        };
        let label = self
            .public_form
            .edit_label_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if label.is_empty() {
            self.public_form.error = Some(Arc::from("Enter an account label"));
            cx.notify();
            return;
        }
        match store.update_public_account_label(
            view_session.as_ref(),
            account_uuid.as_ref(),
            Some(&label),
        ) {
            Ok(_) => {
                self.public_form.editing_account_uuid = None;
                self.reload_public_accounts(window, cx);
                window.close_all_dialogs(cx);
            }
            Err(error) => self.public_form.error = Some(Arc::from(error.to_string())),
        }
        cx.notify();
    }

    pub(super) fn deactivate_public_account(
        &mut self,
        public_account_uuid: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.clone() else {
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            return;
        };
        let neighbour = self.public_account_neighbour(public_account_uuid);
        match store
            .deactivate_derived_public_account(view_session.as_ref(), public_account_uuid.as_ref())
        {
            Ok(_) => {
                if self.public_form.selected_account_uuid.as_deref() == Some(public_account_uuid) {
                    self.public_form.selected_account_uuid = neighbour;
                }
                self.reload_public_accounts(window, cx);
                self.reset_public_asset_focus();
                self.scroll_selected_public_row_into_view(window);
                self.schedule_public_balance_refresh(cx);
            }
            Err(error) => self.public_form.error = Some(Arc::from(error.to_string())),
        }
        cx.notify();
    }

    pub(super) fn activate_public_account(
        &mut self,
        public_account_uuid: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.clone() else {
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            return;
        };
        match store
            .activate_derived_public_account(view_session.as_ref(), public_account_uuid.as_ref())
        {
            Ok(account) => {
                self.public_form.selected_account_uuid =
                    Some(Arc::from(account.public_account_uuid.as_str()));
                self.reload_public_accounts(window, cx);
                self.schedule_public_balance_refresh(cx);
            }
            Err(error) => self.public_form.error = Some(Arc::from(error.to_string())),
        }
        cx.notify();
    }

    pub(super) fn delete_public_account(
        &mut self,
        public_account_uuid: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(account) = self
            .public_account_for_uuid(Some(public_account_uuid))
            .cloned()
        else {
            return;
        };
        if account.is_global() {
            let root = cx.entity();
            let account_uuid = account.public_account_uuid.clone();
            let label = public_account_display_label(&account)
                .unwrap_or_else(|| short_address(&account.address));
            let dialog_width = (window.viewport_size().width * 0.92).min(px(520.0));
            let dialog_max_height = dialog_max_height(window);
            window.open_alert_dialog(cx, move |dialog, _window, _cx| {
                let confirm_root = root.clone();
                let account_uuid = account_uuid.clone();
                confirmation_dialog(
                    dialog,
                    ConfirmationDialogProps::danger(
                        "Delete global account?",
                        "Deleting this global account removes it from every Private wallet.",
                        None,
                        "Delete account",
                    ),
                    dialog_width,
                    dialog_max_height,
                )
                .child(app_strong_text(label.clone()).whitespace_normal())
                .on_ok(move |_event, window, cx| {
                    confirm_root.update(cx, |root, cx| {
                        root.delete_public_account_confirmed(&account_uuid, window, cx);
                    });
                    true
                })
            });
        } else {
            self.delete_public_account_confirmed(public_account_uuid, window, cx);
        }
    }

    fn delete_public_account_confirmed(
        &mut self,
        public_account_uuid: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(account) = self
            .public_account_for_uuid(Some(public_account_uuid))
            .cloned()
        else {
            return;
        };
        let Some(store) = self.vault_store.clone() else {
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            return;
        };
        let neighbour = self.public_account_neighbour(public_account_uuid);
        let account_is_global = account.is_global();
        let owning_wallet_uuid = view_session.wallet_id().to_owned();
        match store
            .delete_imported_public_account(view_session.as_ref(), &account.public_account_uuid)
        {
            Ok(_) => {
                let participation_changed = if account_is_global {
                    remove_global_participant(
                        &mut self.ui_state.governance_participants,
                        &account.public_account_uuid,
                    )
                } else {
                    remove_scoped_participant(
                        &mut self.ui_state.governance_participants,
                        &owning_wallet_uuid,
                        &account.public_account_uuid,
                    )
                };
                if participation_changed {
                    self.save_ui_state();
                    self.invalidate_governance_context();
                }
                if self.public_form.selected_account_uuid.as_deref() == Some(public_account_uuid) {
                    self.public_form.selected_account_uuid = neighbour;
                }
                self.reload_public_accounts(window, cx);
                self.reset_public_asset_focus();
                self.scroll_selected_public_row_into_view(window);
                self.schedule_public_balance_refresh(cx);
            }
            Err(error) => self.public_form.error = Some(Arc::from(error.to_string())),
        }
        cx.notify();
    }

    pub(super) fn public_account_visible_balances(
        &self,
        public_account_uuid: &str,
        status: PublicAccountStatus,
    ) -> Vec<PublicBalanceEntry> {
        public_account_visible_balances_for_chain(
            self.public_balance_snapshot.as_deref(),
            self.selected_chain,
            public_account_uuid,
            status,
        )
    }

    pub(super) fn render_public_add_account_dropdown(
        &self,
        root: &Entity<Self>,
    ) -> impl IntoElement {
        let derive_root = root.clone();
        let import_root = root.clone();
        app_button("wallet-public-add-account-trigger", "Add account")
            .primary()
            .small()
            .dropdown_caret(true)
            .disabled(
                self.vault_store.is_none()
                    || self.view_session.is_none()
                    || self.public_form.adding_account
                    || self.public_form.importing_account,
            )
            .dropdown_menu(move |menu, _window, _cx| {
                let derive_root = derive_root.clone();
                let import_root = import_root.clone();
                menu.min_w(px(190.0))
                    .item(PopupMenuItem::new("Derive from private").on_click(
                        move |_event, window, cx| {
                            derive_root.update(cx, |root, cx| {
                                root.open_public_account_dialog(
                                    PublicAccountDialogKind::Derive,
                                    window,
                                    cx,
                                );
                            });
                        },
                    ))
                    .item(PopupMenuItem::new("Import private key").on_click(
                        move |_event, window, cx| {
                            import_root.update(cx, |root, cx| {
                                root.open_public_account_dialog(
                                    PublicAccountDialogKind::Import,
                                    window,
                                    cx,
                                );
                            });
                        },
                    ))
            })
    }

    pub(super) fn render_public_account_dialog_content(
        &self,
        root: Entity<Self>,
        kind: PublicAccountDialogKind,
        content_width: Pixels,
    ) -> gpui::Div {
        match kind {
            PublicAccountDialogKind::Derive => {
                #[cfg(feature = "hardware")]
                let add_root = root.clone();
                #[cfg(not(feature = "hardware"))]
                let add_root = root;
                let next_index = self.public_form.next_derived_index.map_or_else(
                    || "Next index unavailable".to_string(),
                    |index| format!("Next derived index: {index}"),
                );
                if let Some(device_kind) = self.selected_hardware_public_device_kind() {
                    let hardware_status = self.public_form.hardware_derivation_status;
                    let show_trezor_app_passphrase =
                        self.current_session_needs_trezor_app_passphrase();
                    #[cfg(feature = "hardware")]
                    let trezor_pin_matrix_prompt = self
                        .hardware_profile_unlock
                        .trezor_pin_matrix_prompt
                        .as_ref()
                        .map(|prompt| {
                            super::vault_ui::render_trezor_pin_matrix_prompt(&root, prompt)
                                .into_any_element()
                        });
                    #[cfg(not(feature = "hardware"))]
                    let trezor_pin_matrix_prompt: Option<gpui::AnyElement> = None;
                    let path = self
                        .public_form
                        .next_derived_index
                        .and_then(|public_index| {
                            let wallet_index = self.view_session.as_ref()?.derivation_index();
                            HardwarePublicAccountDescriptor::for_wallet_public_index(
                                device_kind,
                                wallet_index,
                                public_index,
                            )
                            .ok()
                        })
                        .map_or_else(
                            || "Next hardware path unavailable".to_string(),
                            |descriptor| {
                                format!("Next hardware path: {}", descriptor.path_display())
                            },
                        );
                    return div()
                        .w(content_width)
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(app_muted_text(hardware_public_account_setup_copy(
                            device_kind,
                        )))
                        .child(app_muted_text(next_index))
                        .child(app_muted_text(path))
                        .when(show_trezor_app_passphrase, |this| {
                            #[cfg(feature = "hardware")]
                            {
                                this.child(
                                    div()
                                        .w_full()
                                        .p(px(12.0))
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(theme::BORDER))
                                        .bg(rgb(theme::SURFACE))
                                        .child(app_strong_text("Trezor app passphrase"))
                                        .child(
                                            app_muted_text(
                                                "If the Trezor session expired, enter the app passphrase for this account request.",
                                            )
                                            .whitespace_normal(),
                                        )
                                        .child(
                                            super::ui_helpers::input_enter_scope(
                                                !self.public_form.adding_account
                                                    && hardware_status != HardwarePublicAccountDerivationStatus::AwaitingAddressConfirmation,
                                                {
                                                    let root = root.clone();
                                                    move |window, cx| {
                                                        root.update(cx, |root, cx| {
                                                            root.add_public_derived_account_from_input(window, cx);
                                                        });
                                                    }
                                                },
                                            ).child(app_masked_input(
                                                &self.trezor_app_passphrase_input,
                                                self.public_form.adding_account,
                                            )),
                                        ),
                                )
                            }
                            #[cfg(not(feature = "hardware"))]
                            {
                                this
                            }
                        })
                        .children(trezor_pin_matrix_prompt)
                        .child(
                            app_input(&self.public_form.add_label_input)
                                .disabled(self.public_form.adding_account),
                        )
                        .children(self.public_form.error.as_ref().map(|message| {
                            Alert::error("wallet-public-add-derived-error", message.to_string())
                                .small()
                        }))
                        .when(
                            hardware_status == HardwarePublicAccountDerivationStatus::CheckingDevice,
                            |this| this.child(render_hardware_public_account_checking(device_kind)),
                        )
                        .when(
                            hardware_status == HardwarePublicAccountDerivationStatus::AwaitingAddressConfirmation,
                            |this| {
                                this.child(render_hardware_public_account_confirmation_wait(
                                    device_kind,
                                    self.public_form.hardware_confirmation_address,
                                ))
                            },
                        )
                        .when(
                            hardware_status
                                != HardwarePublicAccountDerivationStatus::AwaitingAddressConfirmation,
                            |this| {
                                this.child(
                                    app_button(
                                        "wallet-public-add-derived-submit",
                                        if self.public_form.adding_account {
                                            match hardware_status {
                                                HardwarePublicAccountDerivationStatus::CheckingDevice => {
                                                    format!(
                                                        "Checking {}...",
                                                        hardware_public_device_label(device_kind)
                                                    )
                                                }
                                                _ => "Deriving...".to_owned(),
                                            }
                                        } else {
                                            "Add hardware account".to_owned()
                                        },
                                    )
                                    .primary()
                                    .small()
                                    .loading(self.public_form.adding_account)
                                    .disabled(self.public_form.adding_account)
                                    .on_click(move |_event, window, cx| {
                                        add_root.update(cx, |root, cx| {
                                            root.add_public_derived_account_from_input(window, cx);
                                        });
                                    }),
                                )
                            },
                        );
                }
                div()
                    .w(content_width)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(app_muted_text(
                        "Derive a Public EVM account from the selected Private wallet mnemonic.",
                    ))
                    .child(app_muted_text(next_index))
                    .child(app_input(&self.public_form.add_label_input))
                    .child(app_masked_input(
                        &self.public_form.add_password_input,
                        false,
                    ))
                    .children(self.public_form.error.as_ref().map(|message| {
                        Alert::error("wallet-public-add-derived-error", message.to_string()).small()
                    }))
                    .child(
                        app_button(
                            "wallet-public-add-derived-submit",
                            if self.public_form.adding_account {
                                "Deriving..."
                            } else {
                                "Derive account"
                            },
                        )
                        .primary()
                        .small()
                        .loading(self.public_form.adding_account)
                        .disabled(self.public_form.adding_account)
                        .on_click(move |_event, window, cx| {
                            add_root.update(cx, |root, cx| {
                                root.add_public_derived_account_from_input(window, cx);
                            });
                        }),
                    )
            }
            PublicAccountDialogKind::Import => {
                let import_root = root.clone();
                let global_root = root;
                div()
                    .w(content_width)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(app_muted_text(
                        "Import an EVM private key as a vaulted Public account.",
                    ))
                    .child(app_input(&self.public_form.import_label_input))
                    .child(app_masked_input(
                        &self.public_form.import_private_key_input,
                        false,
                    ))
                    .child(app_masked_input(
                        &self.public_form.import_password_input,
                        false,
                    ))
                    .child(
                        Checkbox::new("wallet-public-import-global")
                            .label("Global account")
                            .checked(self.public_form.import_global)
                            .small()
                            .on_click(move |checked, _window, cx| {
                                let checked = *checked;
                                global_root.update(cx, |root, cx| {
                                    root.public_form.import_global = checked;
                                    cx.notify();
                                });
                            }),
                    )
                    .children(self.public_form.error.as_ref().map(|message| {
                        Alert::error("wallet-public-import-error", message.to_string()).small()
                    }))
                    .child(
                        app_button(
                            "wallet-public-import-submit",
                            if self.public_form.importing_account {
                                "Importing..."
                            } else {
                                "Import account"
                            },
                        )
                        .primary()
                        .small()
                        .loading(self.public_form.importing_account)
                        .disabled(self.public_form.importing_account)
                        .on_click(move |_event, window, cx| {
                            import_root.update(cx, |root, cx| {
                                root.import_public_account_from_input(window, cx);
                            });
                        }),
                    )
            }
            PublicAccountDialogKind::EditLabel => {
                let save_root = root;
                div()
                    .w(content_width)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(app_input(&self.public_form.edit_label_input))
                    .children(self.public_form.error.as_ref().map(|message| {
                        Alert::error("wallet-public-edit-label-error", message.to_string()).small()
                    }))
                    .child(
                        app_button("wallet-public-save-label", "Save")
                            .primary()
                            .small()
                            .on_click(move |_event, window, cx| {
                                save_root.update(cx, |root, cx| {
                                    root.update_selected_public_account_label(window, cx);
                                });
                            }),
                    )
            }
        }
    }
}
