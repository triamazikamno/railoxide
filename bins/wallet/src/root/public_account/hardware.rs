use alloy::primitives::Address;
use futures_util::future::BoxFuture;
use gpui::{ParentElement, Styled, div, px, rgb};
use gpui_component::{IconName, Sizable, spinner::Spinner};
use ui::controls::{app_muted_text, app_strong_text};
use ui::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::hardware::{
    ConfirmedHardwarePublicAccount, HardwareAccountPublicKey, HardwareDeviceKind,
    HardwarePublicAccountDescriptor,
};
use wallet_ops::vault::HardwareProfileSession;
use zeroize::Zeroizing;

use crate::root::ui_helpers::rgb_with_alpha;

#[cfg(feature = "hardware")]
use wallet_ops::hardware::{
    DEFAULT_HARDWARE_DERIVATION_PATH,
    ledger::LedgerHardwareDerivationClient,
    parse_bip32_path,
    trezor::{TrezorHardwareDerivationClient, TrezorPinMatrixProvider},
};

/// What a device request needs from the dialog.
#[cfg_attr(not(feature = "hardware"), allow(dead_code))]
pub(in crate::root) struct HardwareDeviceRequest {
    pub(in crate::root) device_kind: HardwareDeviceKind,
    /// The Private wallet account index, the hardened account level of the Public path.
    pub(in crate::root) wallet_account_index: u32,
    /// The active hardware profile session the connected device must match.
    pub(in crate::root) hardware_session: HardwareProfileSession,
    pub(in crate::root) trezor_app_passphrase: Option<Zeroizing<String>>,
    #[cfg(feature = "hardware")]
    pub(in crate::root) trezor_pin_matrix_provider: Option<TrezorPinMatrixProvider>,
}

/// A device result with the refreshed hardware profile session.
pub(in crate::root) type HardwareDeviceResult<T> =
    BoxFuture<'static, Result<(T, HardwareProfileSession), String>>;

/// The Derive dialog's device operations, so tests can stand in for a device.
pub(in crate::root) trait HardwarePublicAccountDevice: Send + Sync {
    /// Connects, checks the active profile session, and reads the key at
    /// `m/44'/60'/{wallet}'/0` without on-device display.
    fn read_account_key(
        &self,
        request: HardwareDeviceRequest,
    ) -> HardwareDeviceResult<HardwareAccountPublicKey>;

    /// Connects, checks the profile session, shows `descriptor`'s address on the device and
    /// returns the confirmed account.
    fn confirm_account(
        &self,
        request: HardwareDeviceRequest,
        descriptor: HardwarePublicAccountDescriptor,
    ) -> HardwareDeviceResult<ConfirmedHardwarePublicAccount>;
}

/// The connected Ledger or Trezor.
pub(super) struct ConnectedHardwareDevice;

#[cfg(feature = "hardware")]
impl HardwarePublicAccountDevice for ConnectedHardwareDevice {
    fn read_account_key(
        &self,
        request: HardwareDeviceRequest,
    ) -> HardwareDeviceResult<HardwareAccountPublicKey> {
        Box::pin(async move {
            let wallet_account_index = request.wallet_account_index;
            match request.device_kind {
                HardwareDeviceKind::Ledger => {
                    let session = request.hardware_session;
                    let client = connect_ledger(&session).await?;
                    let key = client
                        .account_public_key(wallet_account_index)
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok((key, session))
                }
                HardwareDeviceKind::Trezor => {
                    let (mut client, session) = connect_trezor(request)?;
                    let key = client
                        .account_public_key(wallet_account_index)
                        .map_err(|error| error.to_string())?;
                    Ok((key, session))
                }
            }
        })
    }

    fn confirm_account(
        &self,
        request: HardwareDeviceRequest,
        descriptor: HardwarePublicAccountDescriptor,
    ) -> HardwareDeviceResult<ConfirmedHardwarePublicAccount> {
        Box::pin(async move {
            match request.device_kind {
                HardwareDeviceKind::Ledger => {
                    let session = request.hardware_session;
                    let client = connect_ledger(&session).await?;
                    let confirmed = client
                        .confirmed_public_ethereum_account(&descriptor)
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok((confirmed, session))
                }
                HardwareDeviceKind::Trezor => {
                    let (mut client, session) = connect_trezor(request)?;
                    let confirmed = client
                        .confirmed_public_ethereum_account(&descriptor)
                        .map_err(|error| error.to_string())?;
                    Ok((confirmed, session))
                }
            }
        })
    }
}

#[cfg(not(feature = "hardware"))]
const HARDWARE_UNSUPPORTED: &str = "hardware public account support is not enabled in this build";

#[cfg(not(feature = "hardware"))]
impl HardwarePublicAccountDevice for ConnectedHardwareDevice {
    fn read_account_key(
        &self,
        _request: HardwareDeviceRequest,
    ) -> HardwareDeviceResult<HardwareAccountPublicKey> {
        Box::pin(async { Err(HARDWARE_UNSUPPORTED.to_owned()) })
    }

    fn confirm_account(
        &self,
        _request: HardwareDeviceRequest,
        _descriptor: HardwarePublicAccountDescriptor,
    ) -> HardwareDeviceResult<ConfirmedHardwarePublicAccount> {
        Box::pin(async { Err(HARDWARE_UNSUPPORTED.to_owned()) })
    }
}

/// The `hardware-derive` UI fixture's stand-in Ledger. It reads a fixed account key after the
/// fixture's 2-second beat and never answers a confirmation. Nothing is read from a device.
#[cfg(debug_assertions)]
pub(super) struct UiFixtureHardwareDevice;

#[cfg(debug_assertions)]
impl HardwarePublicAccountDevice for UiFixtureHardwareDevice {
    fn read_account_key(
        &self,
        request: HardwareDeviceRequest,
    ) -> HardwareDeviceResult<HardwareAccountPublicKey> {
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let key = HardwareAccountPublicKey {
                // The compressed secp256k1 generator.
                public_key: alloy::primitives::hex!(
                    "0279BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798"
                )
                .to_vec(),
                chain_code: [0x42; 32],
            };
            Ok((key, request.hardware_session))
        })
    }

    fn confirm_account(
        &self,
        _request: HardwareDeviceRequest,
        _descriptor: HardwarePublicAccountDescriptor,
    ) -> HardwareDeviceResult<ConfirmedHardwarePublicAccount> {
        Box::pin(std::future::pending())
    }
}

#[cfg(feature = "hardware")]
async fn connect_ledger(
    expected: &HardwareProfileSession,
) -> Result<LedgerHardwareDerivationClient, String> {
    let client = LedgerHardwareDerivationClient::connect()
        .await
        .map_err(|error| error.to_string())?;
    let profile_path =
        parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH).map_err(|error| error.to_string())?;
    let active = client
        .active_profile_session(&profile_path)
        .await
        .map_err(|error| error.to_string())?;
    ensure_hardware_public_profile_session(expected, &active)?;
    Ok(client)
}

/// Connects the Trezor in the request's session and returns the session it ended up in.
#[cfg(feature = "hardware")]
fn connect_trezor(
    request: HardwareDeviceRequest,
) -> Result<(TrezorHardwareDerivationClient, HardwareProfileSession), String> {
    let mut hardware_session = request.hardware_session;
    let mut client = TrezorHardwareDerivationClient::connect_with_session(
        hardware_session.trezor_session_id.clone(),
    )
    .map_err(|error| error.to_string())?;
    client.set_passphrase_mode(hardware_session.trezor_passphrase_mode());
    if let Some(passphrase) = request.trezor_app_passphrase {
        client.set_app_passphrase_zeroizing(passphrase);
    }
    if let Some(provider) = request.trezor_pin_matrix_provider {
        client.set_pin_matrix_provider(provider);
    }
    let profile_path =
        parse_bip32_path(DEFAULT_HARDWARE_DERIVATION_PATH).map_err(|error| error.to_string())?;
    let active = client
        .active_profile_session(&profile_path)
        .map_err(|error| error.to_string())?;
    ensure_hardware_public_profile_session(&hardware_session, &active)?;
    hardware_session
        .trezor_session_id
        .clone_from(&active.trezor_session_id);
    hardware_session.set_trezor_passphrase_mode(active.trezor_passphrase_mode());
    Ok((client, hardware_session))
}

pub(super) const fn hardware_public_device_label(device_kind: HardwareDeviceKind) -> &'static str {
    match device_kind {
        HardwareDeviceKind::Ledger => "Ledger",
        HardwareDeviceKind::Trezor => "Trezor",
    }
}

/// The panel shown while the device asks to confirm the address for `index`, the
/// `position`th of `total` selected indexes.
pub(super) fn render_hardware_public_account_confirmation_wait(
    device_kind: HardwareDeviceKind,
    index: u32,
    position: usize,
    total: usize,
    preview_address: Address,
) -> gpui::Div {
    let device_label = hardware_public_device_label(device_kind);
    div()
        .w_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb_with_alpha(theme::SURFACE, 0.72))
        .child(app_strong_text(format!(
            "Confirm the address for index {index} on your {device_label}"
        )))
        .child(app_muted_text(format!("{position} of {total}")))
        .child(
            app_muted_text(format!(
                "Compare this address with the one shown on your {device_label}:"
            ))
            .whitespace_normal(),
        )
        .child(render_hardware_public_confirmation_address(preview_address))
        .child(app_muted_text("Only approve if the addresses match.").whitespace_normal())
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Spinner::new()
                        .icon(IconName::LoaderCircle)
                        .color(rgb(theme::SUCCESS).into())
                        .with_size(px(14.0)),
                )
                .child(
                    app_muted_text(format!("Waiting for {device_label} approval..."))
                        .text_color(rgb(theme::SUCCESS)),
                ),
        )
}

fn render_hardware_public_confirmation_address(preview_address: Address) -> gpui::Div {
    div()
        .w_full()
        .p(px(10.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .child(
            app_muted_text(format!("{preview_address:#x}"))
                .font_family(APP_MONO_FONT_FAMILY)
                .text_size(APP_TEXT_SIZE)
                .whitespace_normal(),
        )
}

pub(in crate::root) fn hardware_public_account_setup_copy(
    device_kind: HardwareDeviceKind,
) -> String {
    let device_label = hardware_public_device_label(device_kind);
    format!(
        "Show the hardware-native Public EVM addresses your {device_label} derives for the selected Private wallet account. Nothing is saved until you add accounts, and you confirm each added address on your {device_label}. Public transactions from these accounts will require device approval."
    )
}

#[cfg(feature = "hardware")]
fn ensure_hardware_public_profile_session(
    expected: &HardwareProfileSession,
    actual: &HardwareProfileSession,
) -> Result<(), String> {
    if expected.device_kind == actual.device_kind && expected.binding == actual.binding {
        Ok(())
    } else {
        Err("wrong hardware device or passphrase context is active".to_owned())
    }
}
