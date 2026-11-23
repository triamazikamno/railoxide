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
    use crate::root::vault::VaultState;
    use crate::root::vault::hardware::{HardwareAccountUnlock, HardwareUnlockWarning};
    use gpui::{AppContext as _, IntoElement, Render, TestAppContext, div};
    use wallet_ops::hardware::HardwareAppVersion;
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

    /// Runs `body` on a fixture root with no open wallet, so a gateway unlock can continue,
    /// and a factory for opened accounts held behind a divergent-secret warning.
    fn with_unlock_fixture(
        cx: &mut TestAppContext,
        body: impl FnOnce(
            &mut WalletRoot,
            &dyn Fn() -> HardwareAccountUnlock,
            &mut Window,
            &mut Context<'_, WalletRoot>,
        ),
    ) {
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
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                let store = root.vault_store.clone().unwrap();
                // A gateway unlock continues only while no wallet is open.
                let current = root.view_session.take().unwrap();
                let unlock = || HardwareAccountUnlock {
                    session: store
                        .load_view_session_with_view_session(&current, "preview")
                        .unwrap(),
                    metadata: store
                        .list_wallet_metadata_for_session(&current, true)
                        .unwrap(),
                    warning: Some(HardwareUnlockWarning::LedgerDivergentSecret(
                        HardwareAppVersion::new(1, 23, 0),
                    )),
                };
                body(root, &unlock, window, cx);
            });
        });
        cx.update(|window, _| window.remove_window());
    }

    #[gpui::test]
    fn hardware_unlock_warning_installs_once_only_on_current_continue(cx: &mut TestAppContext) {
        with_unlock_fixture(cx, |root, unlock, window, cx| {
            let switches = root.wallet_switch_generation;
            // Stands in for the open dialog, whose builder holds the strong lease.
            let lease = Rc::new(Cell::new(true));
            root.hardware_profile_unlock.dialog_lease = Rc::downgrade(&lease);

            let continuation = root.begin_gateway_unlock();
            let generation = root.next_hardware_profile_action_generation();
            root.finish_hardware_account_unlock(unlock(), continuation, generation, window, cx);
            assert!(root.hardware_profile_unlock.unlock_warning.is_some());
            root.dismiss_hardware_profile_unlock_dialog(continuation, window, cx);
            assert!(root.hardware_profile_unlock.unlock_warning.is_none());

            // Dismissal reset the state's lease.
            root.hardware_profile_unlock.dialog_lease = Rc::downgrade(&lease);
            let continuation = root.begin_gateway_unlock();
            let generation = root.next_hardware_profile_action_generation();
            root.finish_hardware_account_unlock(unlock(), continuation, generation, window, cx);
            root.next_hardware_profile_action_generation();
            root.continue_hardware_unlock_warning(window, cx);
            assert!(root.hardware_profile_unlock.unlock_warning.is_none());
            assert_eq!(root.wallet_switch_generation, switches);
            assert!(matches!(root.vault_state, VaultState::ViewUnlocked));

            let continuation = root.begin_gateway_unlock();
            assert!(continuation.is_some());
            let generation = root.next_hardware_profile_action_generation();
            root.finish_hardware_account_unlock(unlock(), continuation, generation, window, cx);
            root.continue_hardware_unlock_warning(window, cx);
            assert!(matches!(root.vault_state, VaultState::SwitchingWallet));
            let switches = root.wallet_switch_generation;
            root.continue_hardware_unlock_warning(window, cx);
            assert_eq!(root.wallet_switch_generation, switches);
            // The installation holds the continuation, so the dialog's dismissal keeps it.
            root.abandon_gateway_unlock_dialog(continuation);
            assert!(root.gateway_unlock_is_current(continuation));
        });
    }

    #[gpui::test]
    fn hardware_unlock_result_is_dropped_once_dialog_is_gone(cx: &mut TestAppContext) {
        with_unlock_fixture(cx, |root, unlock, window, cx| {
            let switches = root.wallet_switch_generation;
            // `close_all_dialogs` drops the dialog without `on_close`, so the action stays
            // current: a later device result is neither held nor installed.
            let lease = Rc::new(Cell::new(true));
            root.hardware_profile_unlock.dialog_lease = Rc::downgrade(&lease);
            drop(lease);
            let continuation = root.begin_gateway_unlock();
            let generation = root.next_hardware_profile_action_generation();
            root.finish_hardware_account_unlock(unlock(), continuation, generation, window, cx);
            assert!(root.hardware_profile_unlock.unlock_warning.is_none());
            let unwarned = HardwareAccountUnlock {
                warning: None,
                ..unlock()
            };
            root.finish_hardware_account_unlock(unwarned, continuation, generation, window, cx);
            assert_eq!(root.wallet_switch_generation, switches);
            assert!(matches!(root.vault_state, VaultState::ViewUnlocked));

            // A warning on screen when that happens is dropped on the next frame.
            let lease = Rc::new(Cell::new(true));
            root.hardware_profile_unlock.dialog_lease = Rc::downgrade(&lease);
            let continuation = root.begin_gateway_unlock();
            let generation = root.next_hardware_profile_action_generation();
            root.finish_hardware_account_unlock(unlock(), continuation, generation, window, cx);
            assert!(root.hardware_profile_unlock.unlock_warning.is_some());
            drop(lease);
            root.hardware_profile_unlock.drop_orphaned_unlock_warning();
            assert!(root.hardware_profile_unlock.unlock_warning.is_none());
            root.continue_hardware_unlock_warning(window, cx);
            assert_eq!(root.wallet_switch_generation, switches);
            assert!(matches!(root.vault_state, VaultState::ViewUnlocked));
        });
    }

    #[gpui::test]
    fn settings_shortcut_cancels_a_browser_hardware_wallet_selection(cx: &mut TestAppContext) {
        with_unlock_fixture(cx, |root, unlock, window, cx| {
            let lease = Rc::new(Cell::new(true));
            root.hardware_profile_unlock.dialog_lease = Rc::downgrade(&lease);
            root.gateway.private_hardware_selection_pending = true;
            let continuation = root.begin_gateway_unlock();
            let generation = root.next_hardware_profile_action_generation();
            root.finish_hardware_account_unlock(unlock(), continuation, generation, window, cx);
            assert!(root.hardware_profile_unlock.unlock_warning.is_some());

            root.open_settings_from_shortcut(window, cx);
            assert!(!root.gateway.private_hardware_selection_pending);
            assert!(!root.gateway_unlock_is_current(continuation));
            assert!(root.hardware_profile_unlock.unlock_warning.is_none());
        });
    }
}
