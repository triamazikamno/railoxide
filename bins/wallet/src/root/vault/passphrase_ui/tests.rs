use super::*;

struct DialogWindow;

impl Render for DialogWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full()
    }
}

#[gpui::test]
fn passphrase_device_auth_cannot_reopen_a_dismissed_workflow(cx: &mut gpui::TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.executor().allow_parking();
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
    root.update(cx, |root, _| {
        root.vault_view_unlock = Some(Arc::new(
            root.vault_store
                .as_ref()
                .unwrap()
                .unlock_view("public list test password")
                .unwrap(),
        ));
    });
    cx.simulate_resize(gpui::size(px(1000.), px(800.)));
    let open_prompt = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                OpenPassphraseWalletAuthorizationUi::open(
                    root,
                    Arc::from("preview"),
                    Arc::from("Public list test"),
                    window,
                    cx,
                )
            })
        })
    };
    let wait_for_open = |cx: &mut gpui::VisualTestContext| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while root.read_with(cx, |root, _| {
            root.pending_software_profile_open.is_none() && root.vault_error.is_none()
        }) {
            assert!(
                std::time::Instant::now() < deadline,
                "passphrase authorization did not finish"
            );
            runtime.block_on(async {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            });
            cx.run_until_parked();
        }
    };
    for dismissal in ["cancel", "settings"] {
        // Model the entity retained by the rendered authorization controls.
        let dialog = open_prompt(cx);
        cx.run_until_parked();
        dialog.update(cx, |dialog, _| dialog.device_auth_pending = true);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let generation = root.read_with(cx, |root, _| {
            root.pending_software_profile_open_operation_generation
        });
        if dismissal == "cancel" {
            let cancel = cx.debug_bounds("open-passphrase-wallet-cancel").unwrap();
            cx.simulate_click(cancel.center(), gpui::Modifiers::none());
        }
        cx.update(|window, cx| {
            if dismissal == "settings" {
                root.update(cx, |root, cx| root.open_settings_from_shortcut(window, cx));
            }
            dialog.update(cx, |dialog, cx| {
                dialog.finish_device_auth(
                    DeviceAuthMethod::TouchId,
                    DeviceAuthPassword::Password(Zeroizing::new(
                        "public list test password".into(),
                    )),
                    window,
                    cx,
                );
            });
        });
        if root.read_with(cx, |root, _| {
            root.pending_software_profile_open_operation_generation != generation
        }) {
            wait_for_open(cx);
        }
        assert!(
            root.read_with(cx, |root, _| root.pending_software_profile_open.is_none()),
            "dismissed prompt started passphrase authorization"
        );
        assert!(root.read_with(cx, |root, _| matches!(
            root.vault_state,
            VaultState::ViewUnlocked
        )));
        cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
    }
    let dialog = open_prompt(cx);
    cx.update(|window, cx| {
        dialog.update(cx, |dialog, cx| {
            dialog.finish_device_auth(
                DeviceAuthMethod::TouchId,
                DeviceAuthPassword::Password(Zeroizing::new("public list test password".into())),
                window,
                cx,
            );
        });
    });
    wait_for_open(cx);
    assert!(root.read_with(cx, |root, _| root.pending_software_profile_open.is_some()));
    assert!(root.read_with(cx, |root, _| matches!(
        root.vault_state,
        VaultState::PendingSoftwareProfileOpen
    )));
    // The unmounted pending view must drop typed secrets when the root abandons pending.
    let ui = root.read_with(cx, |root, _| root.passphrase_open_ui.clone());
    let inputs = ui.read_with(cx, |ui, _| {
        [
            ui.passphrase_input.clone(),
            ui.confirmation_input.clone(),
            ui.label_input.clone(),
        ]
    });
    cx.update(|window, cx| {
        for input in &inputs {
            input.update(cx, |input, cx| input.set_value("typed secret", window, cx));
        }
        root.update(cx, |root, cx| {
            root.abandon_pending_software_profile_open(window, cx);
        });
    });
    cx.run_until_parked();
    for input in &inputs {
        assert!(input.read_with(cx, |input, _| input.value().is_empty()));
    }
    cx.update(|window, cx| {
        assert!(!window.has_active_dialog(cx));
        window.remove_window();
    });
}

#[test]
fn passphrase_input_preserves_case_and_whitespace() {
    let input = "  Mixed Case  ";
    let handoff = exact_passphrase_for_handoff(input).expect("non-empty passphrase");
    assert_eq!(handoff.as_str(), input);
    let whitespace = exact_passphrase_for_handoff(" ").expect("whitespace passphrase");
    assert_eq!(whitespace.as_str(), " ");
}

#[test]
fn labels_are_trimmed_and_duplicate_errors_are_generic() {
    let metadata = WalletMetadataBundle {
        wallet_uuid: "concealed".to_owned(),
        label: "Hidden child".to_owned(),
        derivation_index: 0,
        source: wallet_ops::vault::WalletSource::Imported,
        status: wallet_ops::vault::WalletStatus::Active,
        display_order: 0,
        hardware_descriptor: None,
        hardware_account: None,
        pending_create_new_chain_ids: std::collections::BTreeSet::default(),
        software_context: None,
    };
    assert_eq!(
        validate_pending_context_label("  New wallet  ", std::slice::from_ref(&metadata)),
        Ok("New wallet".to_owned())
    );
    assert_eq!(
        validate_pending_context_label(" hidden CHILD ", std::slice::from_ref(&metadata)),
        Err("That label is already in use")
    );
}

#[test]
fn primary_route_uses_exact_empty_semantics() {
    assert_eq!(
        pending_open_primary_route(""),
        PendingOpenPrimaryRoute::Standard
    );
    assert_eq!(
        pending_open_primary_route(" "),
        PendingOpenPrimaryRoute::Passphrase
    );
}

#[test]
fn remember_standard_context_is_available_only_for_empty_choosing_input() {
    assert!(pending_open_remember_available(
        PendingOpenUiStage::Choosing,
        false,
        false,
    ));
    assert!(!pending_open_remember_available(
        PendingOpenUiStage::Choosing,
        false,
        true,
    ));
    assert!(!pending_open_remember_available(
        PendingOpenUiStage::Choosing,
        true,
        false,
    ));
    assert!(!pending_open_remember_available(
        PendingOpenUiStage::UnknownDecision,
        false,
        false,
    ));
}
