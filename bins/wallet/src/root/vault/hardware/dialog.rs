#[cfg(feature = "hardware")]
use std::{cell::Cell, rc::Rc};

#[cfg(feature = "hardware")]
use gpui::{InteractiveElement, StatefulInteractiveElement};
#[cfg(feature = "hardware")]
use ui::dialog::WalletDialogExt as _;

#[cfg(feature = "hardware")]
use super::{
    Arc, Context, DesktopVaultStore, Focusable, HardwareDerivationError, HardwareDeviceKind,
    HardwareProfilePickerView, HardwareProfileProgressUpdate, HardwareProfileSession,
    HardwareProfileStep, HardwareProfileStepStatus, HardwareProfileUnlockPurpose, ParentElement,
    Styled, TrezorPassphraseMode, TrezorPinMatrixPromptState, TrezorPinMatrixProvider, VaultError,
    ViewUnlock, WalletRoot, Window, WindowExt, Zeroize, Zeroizing, app_strong_text,
    default_hardware_profile_label, default_hardware_profile_steps,
    dismiss_hardware_profile_unlock_state, hardware_device_kind_from_source, hardware_device_label,
    hardware_profile_hardware_error_message, hardware_profile_should_reconnect_after_error,
    hardware_session_needs_trezor_app_passphrase, hardware_wallet_creation_result_is_current, mpsc,
    next_trezor_passphrase_mode, px, secondary_dialog_content_width, trezor_pin_matrix_provider,
    trezor_session_stale_error_message, vault_error_message,
};
#[cfg(not(feature = "hardware"))]
use super::{Context, WalletRoot};

mod trezor_prompts;
mod unlock_flow;
mod unlock_session;

/// Keeps the tracing target of the moved methods at this module's path.
#[cfg(feature = "hardware")]
const LOG_TARGET: &str = module_path!();

#[cfg(all(test, feature = "hardware"))]
mod tests {
    use super::*;
    use gpui::{AppContext as _, IntoElement, Render, TestAppContext, div};
    use wallet_ops::vault::DeviceAuthStatus;

    struct TestWindow;

    impl Render for TestWindow {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    fn hardware_password_unlock_refreshes_device_auth_after_dialog_closes(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        cx.update(gpui_component::init);
        let mut root = None;
        let (_host, cx) = cx.add_window_view(|window, cx| {
            root = Some(crate::root::tests::public_accounts::fixture_root(
                directory.path(),
                &runtime,
                window,
                cx,
            ));
            let view = cx.new(|_| TestWindow);
            gpui_component::Root::new(view, window, cx)
        });
        let root = root.unwrap();
        cx.executor().allow_parking();
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.lock_vault(window, cx);
                root.choose_hardware_wallet(HardwareDeviceKind::Ledger, window, cx);
                let store = root.vault_store.clone().unwrap();
                // Completion must replace this stale cache with the fixture's actual status.
                root.device_auth.touch_id_status = DeviceAuthStatus::NeedsReenrollment;
                root.hardware_profile
                    .password_input
                    .update(cx, |input, cx| {
                        input.set_value("public list test password", window, cx);
                    });
                assert!(
                    root.hardware_profile_vault_view_unlock(&store, window, cx)
                        .is_some()
                );
                window.close_all_dialogs(cx);
            });
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while root.read_with(cx, |root, _| root.device_auth.touch_id_status)
            == DeviceAuthStatus::NeedsReenrollment
        {
            assert!(
                std::time::Instant::now() < deadline,
                "Touch ID status stayed stale"
            );
            runtime.block_on(async {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            });
            cx.run_until_parked();
        }
        root.read_with(cx, |root, _| {
            assert_eq!(
                root.device_auth.touch_id_status,
                root.vault_store
                    .as_ref()
                    .unwrap()
                    .device_auth_status(wallet_ops::device_auth::DeviceAuthMethod::TouchId)
                    .unwrap()
            );
            assert!(root.vault_view_unlock.is_some());
        });
        cx.update(|window, _| window.remove_window());
    }
}
