use super::*;
use gpui::{AppContext as _, Render, TestAppContext};

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

fn account_count(root: &WalletRoot) -> usize {
    root.vault_store
        .as_ref()
        .unwrap()
        .list_all_public_accounts(root.view_session.as_ref().unwrap())
        .unwrap()
        .len()
}

#[gpui::test]
fn device_auth_public_account_ignores_results_after_dismissal_replacement_or_wallet_change(
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
    let before = root.read_with(cx, |root, _| account_count(root));

    for invalidate in ["dismiss", "replace", "wallet"] {
        for outcome in [
            DeviceAuthPassword::Password(Zeroizing::new("public list test password".into())),
            DeviceAuthPassword::Failed(Arc::from("stale prompt failure")),
        ] {
            let (lease, generation) = cx.update(|window, cx| {
                root.update(cx, |root, cx| {
                    let lease = root.open_public_account_dialog(
                        PublicAccountDialogKind::Derive,
                        window,
                        cx,
                    );
                    root.device_auth_in_progress = true;
                    (lease, root.active_wallet_generation)
                })
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            match invalidate {
                "dismiss" => {
                    cx.simulate_keystrokes("escape");
                    cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
                }
                "replace" => {
                    cx.update(|window, cx| {
                        root.update(cx, |root, cx| {
                            root.open_public_account_dialog(
                                PublicAccountDialogKind::Import,
                                window,
                                cx,
                            );
                        });
                    });
                }
                "wallet" => root.update(cx, |root, _| root.advance_active_wallet_generation()),
                _ => unreachable!(),
            }
            cx.update(|window, cx| {
                root.update(cx, |root, cx| {
                    root.public_form.error = Some(Arc::from("current form error"));
                    root.finish_public_account_device_auth(
                        PublicAccountDialogKind::Derive,
                        &lease,
                        generation,
                        outcome,
                        window,
                        cx,
                    );
                    assert!(!root.device_auth_in_progress);
                    assert_eq!(
                        account_count(root),
                        before,
                        "stale authentication created an account"
                    );
                    assert_eq!(
                        root.public_form.error.as_deref(),
                        Some("current form error")
                    );
                    assert_eq!(window.has_active_dialog(cx), invalidate != "dismiss");
                    window.close_all_dialogs(cx);
                });
            });
        }
    }
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn device_auth_public_account_blocks_typed_submission_then_creates_one_account(
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

    for kind in [
        PublicAccountDialogKind::Derive,
        PublicAccountDialogKind::Import,
    ] {
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                let before = account_count(root);
                let lease = root.open_public_account_dialog(kind, window, cx);
                let generation = root.active_wallet_generation;
                for input in [
                    &root.public_form.add_password_input,
                    &root.public_form.import_password_input,
                ] {
                    input.update(cx, |input, cx| {
                        input.set_value("public list test password", window, cx);
                    });
                }
                root.public_form
                    .import_private_key_input
                    .update(cx, |input, cx| input.set_value("11".repeat(32), window, cx));
                root.device_auth_in_progress = true;
                // These are also the entry points used by the input Enter subscriptions.
                match kind {
                    PublicAccountDialogKind::Derive => {
                        root.add_public_derived_account_from_input(window, cx);
                    }
                    PublicAccountDialogKind::Import => {
                        root.import_public_account_from_input(window, cx);
                    }
                    PublicAccountDialogKind::EditLabel => unreachable!(),
                }
                assert_eq!(account_count(root), before);
                assert!(window.has_active_dialog(cx));
                root.finish_public_account_device_auth(
                    kind,
                    &lease,
                    generation,
                    DeviceAuthPassword::Password(Zeroizing::new(
                        "public list test password".into(),
                    )),
                    window,
                    cx,
                );
                assert_eq!(
                    account_count(root),
                    before + 1,
                    "{:?}",
                    root.public_form.error
                );
                assert!(!root.device_auth_in_progress);
                assert!(!window.has_active_dialog(cx));
            });
        });
    }
    cx.update(|window, _| window.remove_window());
}
