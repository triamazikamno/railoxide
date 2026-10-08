use super::*;
use gpui::TestAppContext;

struct DialogWindow;

impl Render for DialogWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .children(crate::root::startup::render_wallet_overlay_layers(
                window, cx,
            ))
    }
}

#[gpui::test]
fn vault_device_auth_ignores_results_after_password_unlock_and_relock(cx: &mut TestAppContext) {
    const PASSWORD: &str = "public list test password";
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
        let view = cx.new(|_| DialogWindow);
        gpui_component::Root::new(view, window, cx)
    });
    let root = root.unwrap();
    cx.executor().allow_parking();
    let wait_for_password_unlock = |cx: &mut gpui::VisualTestContext| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !root.read_with(cx, |root, _| {
            matches!(root.vault_state, VaultState::ViewUnlocked)
        }) {
            assert!(std::time::Instant::now() < deadline, "unlock timed out");
            cx.update(|window, cx| {
                root.update(cx, |root, cx| {
                    if matches!(root.vault_state, VaultState::PendingSoftwareProfileOpen) {
                        root.continue_pending_without_passphrase(false, window, cx);
                    }
                });
            });
            runtime.block_on(async {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            });
            cx.run_until_parked();
        }
        assert!(root.read_with(cx, |root, _| matches!(
            root.vault_state,
            VaultState::ViewUnlocked
        )));
    };

    for outcome in [
        DeviceAuthPassword::Password(Zeroizing::new(PASSWORD.into())),
        DeviceAuthPassword::Failed(Arc::from("old prompt failure")),
    ] {
        let generation = cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.lock_vault(window, cx);
                root.device_auth_in_progress = true;
                let generation = root.active_wallet_generation;
                // The gateway uses this password path while Touch ID is pending.
                root.unlock_vault_with_password(Zeroizing::new(PASSWORD.into()), None, window, cx);
                generation
            })
        });
        wait_for_password_unlock(cx);
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.lock_vault(window, cx);
                root.vault_error = Some(Arc::from("current unlock error"));
                root.focus_vault_input_on_render = false;
                root.finish_vault_device_auth_unlock(generation, outcome, window, cx);
                assert!(!root.device_auth_in_progress);
                assert!(
                    !root.unlock_in_progress,
                    "stale result restarted vault unlock"
                );
                assert!(matches!(root.vault_state, VaultState::UnlockVault));
                assert!(root.vault_view_unlock.is_none());
                assert_eq!(root.vault_error.as_deref(), Some("current unlock error"));
                assert!(!root.focus_vault_input_on_render);
            });
        });
        cx.run_until_parked();
        assert!(root.read_with(cx, |root, _| matches!(
            root.vault_state,
            VaultState::UnlockVault
        )));
    }

    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.device_auth_in_progress = true;
            root.finish_vault_device_auth_unlock(
                root.active_wallet_generation,
                DeviceAuthPassword::Password(Zeroizing::new(PASSWORD.into())),
                window,
                cx,
            );
        });
    });
    wait_for_password_unlock(cx);
    cx.update(|window, _| window.remove_window());
}

#[cfg(feature = "hardware")]
#[gpui::test]
fn hardware_device_auth_ignores_results_after_dialog_replacement(cx: &mut TestAppContext) {
    use wallet_ops::hardware::HardwareDeviceKind;

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
        let view = cx.new(|_| DialogWindow);
        gpui_component::Root::new(view, window, cx)
    });
    let root = root.unwrap();
    cx.simulate_resize(gpui::size(px(1000.), px(800.)));
    cx.update(|window, cx| root.update(cx, |root, cx| root.lock_vault(window, cx)));

    for replacement in ["hardware", "settings"] {
        for outcome in [
            DeviceAuthPassword::Password(Zeroizing::new("public list test password".into())),
            DeviceAuthPassword::Failed(Arc::from("old prompt failure")),
        ] {
            cx.update(|window, cx| {
                root.update(cx, |root, cx| {
                    root.choose_hardware_wallet(HardwareDeviceKind::Ledger, window, cx);
                });
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let (lease, generation) = root.update(cx, |root, _| {
                root.device_auth_in_progress = true;
                (
                    root.hardware_profile_unlock.dialog_lease.clone(),
                    root.hardware_wallet_creation_generation,
                )
            });
            if replacement == "hardware" {
                cx.simulate_keystrokes("escape");
                cx.update(|window, cx| {
                    assert!(!window.has_active_dialog(cx));
                    root.update(cx, |root, cx| {
                        root.choose_hardware_wallet(HardwareDeviceKind::Ledger, window, cx);
                    });
                });
            } else {
                cx.update(|window, cx| {
                    root.update(cx, |root, cx| root.open_settings_from_shortcut(window, cx));
                });
            }
            cx.run_until_parked();
            cx.update(|window, cx| {
                root.update(cx, |root, cx| {
                    root.hardware_profile_password_input
                        .update(cx, |input, cx| {
                            input.set_value("replacement typed password", window, cx);
                        });
                    root.hardware_profile_unlock.error = Some(Arc::from("replacement error"));
                    root.finish_hardware_profile_device_auth_unlock(
                        &lease, generation, outcome, window, cx,
                    );
                    assert!(!root.device_auth_in_progress);
                    assert!(!root.hardware_profile_unlock.in_progress);
                    assert!(root.hardware_profile_unlock.vault_view_unlock.is_none());
                    assert!(root.vault_view_unlock.is_none());
                    assert_eq!(
                        root.hardware_profile_unlock.error.as_deref(),
                        Some("replacement error")
                    );
                    assert_eq!(
                        root.hardware_profile_password_input.read(cx).value(),
                        "replacement typed password"
                    );
                    assert!(window.has_active_dialog(cx));
                    window.close_all_dialogs(cx);
                });
            });
        }
    }

    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.choose_hardware_wallet(HardwareDeviceKind::Ledger, window, cx);
            root.device_auth_in_progress = true;
            let lease = root.hardware_profile_unlock.dialog_lease.clone();
            root.finish_hardware_profile_device_auth_unlock(
                &lease,
                root.hardware_wallet_creation_generation,
                DeviceAuthPassword::Failed(Arc::from("current prompt failure")),
                window,
                cx,
            );
            assert_eq!(
                root.hardware_profile_unlock.error.as_deref(),
                Some("current prompt failure")
            );
            window.close_all_dialogs(cx);
        });
    });
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn add_wallet_device_auth_ignores_results_after_dismissal_reopening_or_wallet_change(
    cx: &mut TestAppContext,
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
        let view = cx.new(|_| DialogWindow);
        gpui_component::Root::new(view, window, cx)
    });
    let root = root.unwrap();
    cx.simulate_resize(gpui::size(px(1000.), px(800.)));

    for invalidate in ["dismiss", "reopen", "wallet"] {
        for outcome in [
            DeviceAuthPassword::Password(Zeroizing::new("old authentication".into())),
            DeviceAuthPassword::Failed(Arc::from("old prompt failure")),
        ] {
            let failed = matches!(&outcome, DeviceAuthPassword::Failed(_));
            cx.update(|window, cx| {
                root.update(cx, |root, cx| {
                    root.open_add_wallet_dialog(window, cx);
                });
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let (lease, generation) = root.update(cx, |root, _| {
                root.device_auth_in_progress = true;
                (
                    root.add_wallet_dialog_lease.clone(),
                    root.active_wallet_generation,
                )
            });
            match invalidate {
                "dismiss" => cx.simulate_keystrokes("escape"),
                "reopen" => {
                    cx.update(|window, cx| {
                        root.update(cx, |root, cx| {
                            root.open_add_wallet_dialog(window, cx);
                        });
                    });
                    cx.run_until_parked();
                }
                "wallet" => root.update(cx, |root, _| root.advance_active_wallet_generation()),
                _ => unreachable!(),
            }
            cx.update(|window, cx| {
                root.update(cx, |root, cx| {
                    root.add_wallet_password_input.update(cx, |input, cx| {
                        input.set_value("current typed password", window, cx);
                    });
                    root.vault_error = Some(Arc::from("current form error"));
                    root.touch_id_status = DeviceAuthStatus::Enabled;
                    root.finish_add_wallet_device_auth(&lease, generation, outcome, window, cx);
                    assert!(!root.device_auth_in_progress);
                    assert!(
                        root.add_wallet_device_auth_password.is_none(),
                        "stale result authorized the current form"
                    );
                    assert_eq!(
                        root.add_wallet_password_input.read(cx).value(),
                        "current typed password"
                    );
                    assert_eq!(root.vault_error.as_deref(), Some("current form error"));
                    if failed {
                        assert_eq!(
                            root.touch_id_status,
                            root.vault_store
                                .as_ref()
                                .unwrap()
                                .device_auth_status(
                                    wallet_ops::device_auth::DeviceAuthMethod::TouchId
                                )
                                .unwrap()
                        );
                    }
                    assert_eq!(window.has_active_dialog(cx), invalidate != "dismiss");
                    window.close_all_dialogs(cx);
                });
            });
        }
    }

    cx.update(|window, cx| root.update(cx, |root, cx| root.open_add_wallet_dialog(window, cx)));
    cx.run_until_parked();
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            let lease = root.add_wallet_dialog_lease.clone();
            root.device_auth_in_progress = true;
            root.finish_add_wallet_device_auth(
                &lease,
                root.active_wallet_generation,
                DeviceAuthPassword::Password(Zeroizing::new("current authentication".into())),
                window,
                cx,
            );
            assert!(
                root.add_wallet_device_auth_password
                    .as_ref()
                    .is_some_and(|password| password.as_str() == "current authentication")
            );
            assert!(!root.device_auth_in_progress);
            window.close_all_dialogs(cx);
        });
    });
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn device_auth_enrollment_vetoes_cancel_until_completion(cx: &mut TestAppContext) {
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
        let view = cx.new(|_| DialogWindow);
        gpui_component::Root::new(view, window, cx)
    });
    let root = root.unwrap();
    cx.simulate_resize(gpui::size(px(1000.), px(800.)));

    for result in [Err(Arc::from("enrollment failed")), Ok(())] {
        let dialog = cx.update(|window, cx| {
            root.update(cx, |_, cx| {
                EnableDeviceAuthDialogContent::open(DeviceAuthMethod::TouchId, window, cx)
            })
        });
        cx.run_until_parked();
        let (send, receive) = tokio::sync::oneshot::channel();
        cx.update(|window, cx| {
            dialog.update(cx, |dialog, cx| {
                dialog.observe_enrollment(async move { receive.await.unwrap() }, window, cx);
            });
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let cancel = cx.debug_bounds("wallet-enable-touch-id-cancel").unwrap();
        cx.simulate_click(cancel.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("escape");
        cx.simulate_click(gpui::point(px(5.), px(5.)), gpui::Modifiers::none());
        cx.update(|window, cx| assert!(window.has_active_dialog(cx)));

        let failed = result.is_err();
        send.send(result).unwrap();
        cx.run_until_parked();
        assert!(!dialog.read_with(cx, |dialog, _| dialog.pending));
        cx.update(|window, cx| assert_eq!(window.has_active_dialog(cx), failed));
        if failed {
            assert!(dialog.read_with(cx, |dialog, _| dialog.error.is_some()));
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let cancel = cx.debug_bounds("wallet-enable-touch-id-cancel").unwrap();
            cx.simulate_click(cancel.center(), gpui::Modifiers::none());
            cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
        }
    }
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn device_auth_enrollment_refreshes_root_without_closing_or_focusing_a_newer_dialog(
    cx: &mut TestAppContext,
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
        let view = cx.new(|_| DialogWindow);
        gpui_component::Root::new(view, window, cx)
    });
    let root = root.unwrap();
    cx.simulate_resize(gpui::size(px(1000.), px(800.)));

    for (replace, retain, result) in [
        (true, false, Err(Arc::from("enrollment failed"))),
        (true, true, Ok(())),
        (false, true, Ok(())),
        (false, true, Err(Arc::from("enrollment failed"))),
    ] {
        let dialog = cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                // Deliberately stale cache: completion must reread the disabled test vault.
                root.touch_id_status = DeviceAuthStatus::Enabled;
                EnableDeviceAuthDialogContent::open(DeviceAuthMethod::TouchId, window, cx)
            })
        });
        cx.run_until_parked();
        let (send, receive) = tokio::sync::oneshot::channel();
        let replacement_focus = cx.update(|window, cx| {
            dialog.update(cx, |dialog, cx| {
                dialog.observe_enrollment(async move { receive.await.unwrap() }, window, cx);
            });
            if replace {
                window.close_all_dialogs(cx);
            }
            window.open_dialog(cx, |dialog, _, _| dialog.title("Newer dialog"));
            window.focused(cx).unwrap()
        });
        let old = dialog.downgrade();
        // Keep one closed entity alive to prove that entity lifetime is not dialog lifetime.
        let retained = retain.then_some(dialog);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        if !retain {
            assert!(old.upgrade().is_none());
        }
        send.send(result).unwrap();
        cx.run_until_parked();
        assert_eq!(
            root.read_with(cx, |root, _| root.touch_id_status),
            DeviceAuthStatus::Disabled
        );
        cx.update(|window, cx| {
            assert!(window.has_active_dialog(cx));
            assert!(replacement_focus.is_focused(window));
            window.close_dialog(cx);
            assert_eq!(window.has_active_dialog(cx), !replace);
            window.close_all_dialogs(cx);
        });
        drop(retained);
    }
    cx.update(|window, _| window.remove_window());
}

#[gpui_kit::test]
fn device_auth_settings_preserve_drafts_and_follow_enrollment(cx: &mut TestAppContext) {
    use crate::root::settings::WalletSettingsEditor;
    use gpui_kit::test::TestWindowExt as _;

    struct SettingsWindow(Entity<WalletSettingsEditor>);
    impl Render for SettingsWindow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone()).children(
                crate::root::startup::render_wallet_overlay_layers(window, cx),
            )
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_kit::init);
    cx.update(crate::root::install_wallet_action_bindings);
    let mut root = None;
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let wallet = crate::root::tests::public_accounts::fixture_root(
            directory.path(),
            &runtime,
            window,
            cx,
        );
        wallet.update(cx, |root, _| {
            root.touch_id_supported = true;
            root.apple_watch_supported = true;
            root.touch_id_available = false;
            root.apple_watch_available = false;
        });
        let editor = wallet.read(cx).settings_editor.clone().unwrap();
        editor.update(cx, |editor, cx| {
            editor.draft.runtime.auto_lock_timeout_secs = Some(600);
            editor.programmatic_draft_changed(cx);
        });
        root = Some(wallet);
        let view = cx.new(|_| SettingsWindow(editor));
        gpui_component::Root::new(view, window, cx)
    });
    let root = root.unwrap();
    let editor = root.read_with(cx, |root, _| root.settings_editor.clone().unwrap());
    let draft = editor.read_with(cx, |editor, _| editor.draft.clone());
    cx.simulate_resize(gpui::size(px(1000.), px(800.)));
    for (method, switch_id, submit_id, cancel_id) in [
        (
            DeviceAuthMethod::TouchId,
            "wallet-settings-touch-id",
            "wallet-enable-touch-id-submit",
            "wallet-enable-touch-id-cancel",
        ),
        (
            DeviceAuthMethod::AppleWatch,
            "wallet-settings-apple-watch",
            "wallet-enable-apple-watch-submit",
            "wallet-enable-apple-watch-cancel",
        ),
    ] {
        // Supported hardware may be temporarily unusable. Do not ask for the
        // vault password until macOS reports that the method can authenticate.
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(switch_id, cx);
            assert!(!window.has_active_dialog(cx));
            assert_ne!(window.find(switch_id).focused(), Some(true));
            assert_eq!(editor.read(cx).draft, draft);
            root.update(cx, |root, cx| {
                match method {
                    DeviceAuthMethod::TouchId => root.touch_id_available = true,
                    DeviceAuthMethod::AppleWatch => root.apple_watch_available = true,
                }
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find(switch_id).checked(), Some(false));
            window.click(switch_id, cx);
            assert!(window.has_active_dialog(cx));
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.find(submit_id).visible());
            assert_eq!(window.find(switch_id).checked(), Some(false));
            window.click(cancel_id, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(!window.has_active_dialog(cx));
            assert_eq!(window.find(switch_id).focused(), Some(true));
        });
        let keystroke = gpui::Keystroke::parse("space").unwrap();
        cx.simulate_event(gpui::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(gpui::KeyUpEvent { keystroke });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            // Invalid enrollment must leave both the setting and the draft unchanged.
            window.click(submit_id, cx);
            assert!(window.has_active_dialog(cx));
            assert_eq!(window.find(switch_id).checked(), Some(false));
            window.click(cancel_id, cx);
            assert_eq!(editor.read(cx).draft, draft);
        });
    }

    // Completion notifies the wallet root. Settings must observe that owner without
    // copying the enrollment flag into its Save/Discard draft.
    for status in [
        DeviceAuthStatus::Enabled,
        DeviceAuthStatus::NeedsReenrollment,
    ] {
        cx.update(|_, cx| {
            root.update(cx, |root, cx| {
                root.touch_id_status = status;
                root.apple_watch_status = status;
                // Saved settings remain visible while either device is unavailable.
                root.touch_id_supported = false;
                root.apple_watch_supported = false;
                root.touch_id_available = false;
                root.apple_watch_available = false;
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("wallet-settings-touch-id").checked(),
                Some(true)
            );
            assert_eq!(
                window.find("wallet-settings-apple-watch").checked(),
                Some(true)
            );
            assert_eq!(editor.read(cx).draft, draft);
        });
    }
    cx.update(|window, cx| {
        window.click("wallet-settings-discard", cx);
        assert!(!editor.read(cx).is_dirty());
        assert_eq!(
            window.find("wallet-settings-touch-id").checked(),
            Some(true)
        );
        window.click("wallet-settings-touch-id", cx);
        assert!(!window.has_active_dialog(cx));
        assert_eq!(root.read(cx).touch_id_status, DeviceAuthStatus::Disabled);
        assert!(!editor.read(cx).is_dirty());
        window.remove_window();
    });
}
