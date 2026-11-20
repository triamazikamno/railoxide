use std::{
    cell::Cell,
    rc::{Rc, Weak},
    sync::Arc,
};

use gpui::{
    AppContext as _, Context, Entity, Focusable, InteractiveElement as _, IntoElement,
    ParentElement, Pixels, SharedString, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Disableable, Sizable, WindowExt,
    alert::Alert,
    button::{ButtonGroup, ButtonVariants},
    checkbox::Checkbox,
    input::InputGroupButton,
    menu::{DropdownMenu, PopupMenuItem},
};
use railgun_ui::chain_name;
use ui::controls::{
    app_button, app_input, app_masked_input, app_muted_text, app_segment_button, app_strong_text,
};
use ui::dialog::WalletDialogExt as _;
use wallet_ops::{
    PublicAssetId, PublicBalanceEntry,
    hardware::HardwareDeviceKind,
    vault::{
        PublicAccountMetadata, PublicAccountStatus, WalletSource, public_account_default_label,
    },
};
use zeroize::Zeroizing;

mod account_dialogs;
mod account_import;
mod account_management;
mod account_render;
mod account_selection;
mod assets;
mod commands;
mod components;
mod derive_preview;
mod device_auth_submit;
#[cfg(test)]
mod device_auth_tests;
mod hardware;
mod identicon;
pub(super) mod list;
mod qr;
mod tron;
mod tron_send;
mod types;
mod wallet_state;

/// Keeps the tracing target of the moved methods at this module's path.
const LOG_TARGET: &str = module_path!();

pub(super) use components::{
    next_public_account_label_number, public_account_display_label, public_account_matches_search,
    public_account_source_label,
};
#[cfg(test)]
pub(super) use hardware::hardware_public_account_setup_copy;
#[cfg(test)]
pub(super) use identicon::{
    PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT, PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE,
    public_account_identicon_color, public_account_identicon_pattern,
};
pub(super) use qr::{public_address_qr_payload, render_public_address_qr_dialog_content};
pub(super) use tron::TronPublicState;
pub(super) use tron_send::{TronSendIntent, TronSendState};
pub(super) use types::PublicAccountFormState;
#[cfg(test)]
pub(super) use ui::public_address::{
    PUBLIC_ADDRESS_QR_QUIET_ZONE_MODULES, public_address_qr_module_range,
};

use super::device_auth::{
    DEVICE_AUTH_REASON_PUBLIC_ACCOUNT, DeviceAuthMethod, DeviceAuthPassword, device_auth_buttons,
    masked_input_with_device_auth,
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
