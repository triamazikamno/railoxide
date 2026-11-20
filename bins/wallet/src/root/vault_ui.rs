#[cfg(feature = "hardware")]
use std::sync::Arc;

use gpui::{
    Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, img, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::alert::Alert;
use gpui_component::checkbox::Checkbox;
use gpui_component::progress::Progress as UiProgress;
use gpui_component::spinner::Spinner;
use gpui_component::{
    Disableable, IconName, WindowExt,
    button::{Button, ButtonVariants},
    tooltip::Tooltip,
};
use gpui_component::{Icon, Sizable};
#[cfg(feature = "hardware")]
use gpui_component::{Selectable, button::ButtonGroup};
use ui::clipboard::secret_clipboard_with_toast;
use ui::controls::{
    app_button, app_button_base, app_button_label, app_input, app_masked_input,
    app_muted_shimmer_text, app_muted_text, app_strong_text,
};
use ui::dialog::WalletDialogExt as _;
#[cfg(feature = "hardware")]
use ui::theme::APP_MONO_FONT_FAMILY;
use ui::theme::{self, APP_TEXT_SIZE};
use wallet_ops::hardware::{HardwareDeviceKind, HardwareWalletSyncIntent};
#[cfg(feature = "hardware")]
use wallet_ops::vault::TrezorPassphraseMode;

#[cfg(feature = "hardware")]
use super::actions::{CycleTrezorPassphraseMode, TREZOR_PASSPHRASE_MODE_KEY_CONTEXT};
use super::device_auth::{device_auth_buttons, masked_input_with_device_auth};
use super::settings::{data_folder_path, data_folder_row, settings_dialog_dimensions};
use super::shell::render_wallet_hero_screen;
#[cfg(feature = "hardware")]
use super::vault::{
    HardwareProfileApprovalPrompt, HardwareProfilePickerView, HardwareProfileStep,
    HardwareProfileStepState, HardwareProfileStepStatus,
};
use super::vault::{PendingSoftwareProfileOpenStage, hardware_device_label};
use super::{Activity, VaultState, WalletRoot, WalletSetupMode, labeled_field, rgb_with_alpha};
#[cfg(feature = "hardware")]
use crate::assets::RailgunActionIcon;
use crate::assets::{
    IMPORT_ICON_PATH, LEDGER_LOGO_SHORT_WHITE_ICON_PATH, TREZOR_SYMBOL_WHITE_ICON_PATH,
};

#[cfg(feature = "hardware")]
mod trezor;
#[cfg(feature = "hardware")]
use trezor::trezor_pin_matrix_title;

#[cfg(test)]
mod tests;

mod hardware_profile_dialog;
#[cfg(feature = "hardware")]
mod hardware_profile_picker;
mod hardware_profile_render;
mod vault_screens;
mod wallet_setup;

pub(super) use hardware_profile_render::*;

pub(super) const fn should_show_pre_unlock_settings_action(vault_state: &VaultState) -> bool {
    matches!(
        vault_state,
        VaultState::CreateVault
            | VaultState::UnlockVault
            | VaultState::SetupWallet
            | VaultState::Error(_)
    )
}

const fn pending_software_profile_open_title(
    stage: Option<PendingSoftwareProfileOpenStage>,
) -> &'static str {
    match stage {
        None | Some(PendingSoftwareProfileOpenStage::Choosing) => "Open wallet",
        Some(PendingSoftwareProfileOpenStage::UnknownDecision) => "Passphrase wallet not found",
        Some(PendingSoftwareProfileOpenStage::CreationHandoff) => "Add passphrase wallet",
    }
}

pub(in crate::root) fn vault_dialog_body(subtitle: impl Into<SharedString>) -> gpui::Div {
    let subtitle = subtitle.into();
    div().w_full().flex().flex_col().gap_3().child(
        app_muted_text(subtitle)
            .whitespace_normal()
            .line_height(px(18.0)),
    )
}

pub(in crate::root) const fn hardware_create_button_id(
    device_kind: HardwareDeviceKind,
) -> &'static str {
    match device_kind {
        HardwareDeviceKind::Ledger => "create-ledger-derived-wallet",
        HardwareDeviceKind::Trezor => "create-trezor-derived-wallet",
    }
}

pub(in crate::root) const fn hardware_recover_button_id(
    device_kind: HardwareDeviceKind,
) -> &'static str {
    match device_kind {
        HardwareDeviceKind::Ledger => "recover-ledger-derived-wallet",
        HardwareDeviceKind::Trezor => "recover-trezor-derived-wallet",
    }
}

fn setup_button_label(icon: gpui::AnyElement, label: &'static str) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_center()
        .gap_2()
        .child(setup_icon_slot(icon))
        .child(app_button_label(label))
}

fn setup_icon_slot(icon: gpui::AnyElement) -> gpui::Div {
    div()
        .size(px(22.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(icon)
}

fn setup_component_icon(icon: IconName) -> gpui::AnyElement {
    Icon::new(icon).size(px(22.0)).into_any_element()
}

fn setup_embedded_icon(path: &'static str) -> gpui::AnyElement {
    Icon::empty().path(path).size(px(22.0)).into_any_element()
}
