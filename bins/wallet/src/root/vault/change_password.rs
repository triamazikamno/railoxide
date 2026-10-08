use std::{
    cell::Cell,
    rc::{Rc, Weak},
    sync::Arc,
};

use gpui::{
    AppContext, Context, Entity, Focusable, IntoElement, ParentElement, Render, Styled, Window,
    div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, WindowExt,
    button::ButtonVariants,
    input::{InputEvent, InputGroupButton, InputState},
};
use ui::controls::{app_button, app_muted_text, app_strong_text};
use ui::theme;
use wallet_ops::vault::VaultError;
use zeroize::Zeroizing;

use super::super::device_auth::{
    DEVICE_AUTH_REASON_CHANGE_PASSWORD, DeviceAuthMethod, DeviceAuthPassword, DeviceAuthPrompt,
    device_auth_buttons, masked_input_with_device_auth,
};
use super::super::{WalletRoot, new_masked_input, secondary_dialog_content_width};
use super::VaultState;

const CHANGE_VAULT_PASSWORD_DIALOG_WIDTH: gpui::Pixels = px(460.0);

struct ChangeVaultPasswordDialogContent {
    root: Entity<WalletRoot>,
    current_password_input: Entity<InputState>,
    new_password_input: Entity<InputState>,
    confirm_password_input: Entity<InputState>,
    pending: bool,
    error: Option<Arc<str>>,
    device_auth: Option<DeviceAuthPrompt>,
    device_auth_pending: bool,
    lease: Weak<Cell<bool>>,
    wallet_generation: u64,
}

#[derive(Clone, Copy)]
enum ChangeVaultPasswordEnterAction {
    FocusNewPassword,
    FocusConfirmPassword,
    Submit,
}

impl ChangeVaultPasswordDialogContent {
    fn open(
        root: &mut WalletRoot,
        window: &mut Window,
        cx: &mut Context<'_, WalletRoot>,
    ) -> Entity<Self> {
        window.close_all_dialogs(cx);
        let entity = cx.entity();
        let device_auth = root.device_auth_prompt();
        let lease = Rc::new(Cell::new(true));
        let identity = Rc::downgrade(&lease);
        let wallet_generation = root.active_wallet_generation;
        let content =
            cx.new(|cx| Self::new(entity, device_auth, identity, wallet_generation, window, cx));
        let focus_content = content.clone();
        let dialog_content = content.clone();
        let dialog_width =
            (window.viewport_size().width * 0.92).min(CHANGE_VAULT_PASSWORD_DIALOG_WIDTH);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let identity = Rc::downgrade(&lease);
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .title(app_strong_text("Change vault password"))
                .on_close(move |_, _, _| {
                    if let Some(open) = identity.upgrade() {
                        open.set(false);
                    }
                })
                .child(div().w(content_width).child(dialog_content.clone()))
        });
        cx.defer_in(window, move |_root, window, cx| {
            focus_content.update(cx, |content, cx| {
                content.focus_current_password(window, cx);
            });
        });
        content
    }

    fn new(
        root: Entity<WalletRoot>,
        device_auth: Option<DeviceAuthPrompt>,
        lease: Weak<Cell<bool>>,
        wallet_generation: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let current_password_input = new_masked_input(window, cx, "current vault password");
        let new_password_input = new_masked_input(window, cx, "new vault password");
        let confirm_password_input = new_masked_input(window, cx, "confirm new vault password");
        for (input, enter_action) in [
            (
                current_password_input.clone(),
                ChangeVaultPasswordEnterAction::FocusNewPassword,
            ),
            (
                new_password_input.clone(),
                ChangeVaultPasswordEnterAction::FocusConfirmPassword,
            ),
            (
                confirm_password_input.clone(),
                ChangeVaultPasswordEnterAction::Submit,
            ),
        ] {
            cx.subscribe_in(
                &input,
                window,
                move |this, _input, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => {
                        this.handle_enter(enter_action, window, cx);
                    }
                    InputEvent::Change => {
                        this.error = None;
                        cx.notify();
                    }
                    _ => {}
                },
            )
            .detach();
        }

        Self {
            root,
            current_password_input,
            new_password_input,
            confirm_password_input,
            pending: false,
            error: None,
            device_auth,
            device_auth_pending: false,
            lease,
            wallet_generation,
        }
    }

    fn is_current(&self, cx: &Context<'_, Self>) -> bool {
        let root = self.root.read(cx);
        self.lease.upgrade().is_some_and(|open| open.get())
            && root.active_wallet_generation == self.wallet_generation
            && matches!(root.vault_state, VaultState::ViewUnlocked)
    }

    fn focus_current_password(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.current_password_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    fn handle_enter(
        &mut self,
        enter_action: ChangeVaultPasswordEnterAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.pending || self.device_auth_pending || !self.is_current(cx) {
            return;
        }
        match enter_action {
            ChangeVaultPasswordEnterAction::FocusNewPassword => {
                self.new_password_input
                    .read(cx)
                    .focus_handle(cx)
                    .focus(window, cx);
            }
            ChangeVaultPasswordEnterAction::FocusConfirmPassword => {
                self.confirm_password_input
                    .read(cx)
                    .focus_handle(cx)
                    .focus(window, cx);
            }
            ChangeVaultPasswordEnterAction::Submit => self.submit(window, cx),
        }
    }

    fn submit(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if self.pending || self.device_auth_pending || !self.is_current(cx) {
            return;
        }
        let current_password =
            Zeroizing::new(self.current_password_input.read(cx).value().to_string());
        if current_password.trim().is_empty() {
            self.error = Some(Arc::from("Enter the current vault password"));
            cx.notify();
            return;
        }
        self.submit_with_current_password(current_password, window, cx);
    }

    /// Uses the selected device for the current password once the new password is valid.
    fn submit_with_device_auth(
        &mut self,
        method: DeviceAuthMethod,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.pending || self.device_auth_pending || !self.is_current(cx) {
            return;
        }
        let Some(prompt) = self.device_auth.clone() else {
            return;
        };
        if let Err(message) = self.new_password(cx) {
            self.error = Some(message);
            cx.notify();
            return;
        }
        self.device_auth_pending = true;
        self.error = None;
        cx.notify();
        prompt.run(
            method,
            DEVICE_AUTH_REASON_CHANGE_PASSWORD,
            window,
            cx,
            move |dialog, outcome, window, cx| {
                dialog.finish_device_auth(method, outcome, window, cx);
            },
        );
    }

    fn finish_device_auth(
        &mut self,
        method: DeviceAuthMethod,
        outcome: DeviceAuthPassword,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.device_auth_pending = false;
        cx.notify();
        if !self.is_current(cx) {
            return;
        }
        match outcome {
            DeviceAuthPassword::Password(current_password) => {
                self.submit_with_current_password(current_password, window, cx);
            }
            DeviceAuthPassword::Cancelled => {}
            DeviceAuthPassword::Failed(message) => {
                self.device_auth = self
                    .device_auth
                    .take()
                    .and_then(|prompt| prompt.without(method));
                self.error = Some(message);
                self.focus_current_password(window, cx);
            }
        }
    }

    fn new_password(&self, cx: &Context<'_, Self>) -> Result<Zeroizing<String>, Arc<str>> {
        let new_password = Zeroizing::new(self.new_password_input.read(cx).value().to_string());
        let confirm_password =
            Zeroizing::new(self.confirm_password_input.read(cx).value().to_string());
        if new_password.trim().is_empty() {
            return Err(Arc::from("Enter a new vault password"));
        }
        if new_password.as_str() != confirm_password.as_str() {
            return Err(Arc::from("New vault passwords do not match"));
        }
        Ok(new_password)
    }

    fn submit_with_current_password(
        &mut self,
        current_password: Zeroizing<String>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.is_current(cx) {
            return;
        }
        let new_password = match self.new_password(cx) {
            Ok(new_password) => new_password,
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                return;
            }
        };
        if current_password.as_str() == new_password.as_str() {
            self.error = Some(Arc::from(
                "Choose a new password that is different from the current password",
            ));
            cx.notify();
            return;
        }

        let start = self.root.update(cx, move |root, _cx| {
            let Some(store) = root.vault_store.clone() else {
                return Err(Arc::from("Wallet vault storage is unavailable"));
            };
            Ok(root.runtime.spawn_blocking(move || {
                store.reencrypt_vault(current_password.as_str(), new_password.as_str())
            }))
        });
        let join = match start {
            Ok(join) => join,
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                return;
            }
        };

        self.pending = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.pending = false;
                match result {
                    Ok(Ok(())) => {
                        dialog.clear_inputs(window, cx);
                        let root = dialog.root.clone();
                        root.update(cx, |root, cx| {
                            root.clear_spend_authorization(cx);
                            root.refresh_device_auth_status();
                        });
                        window.close_dialog(cx);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "vault password change failed");
                        dialog.error = Some(change_vault_password_error_message(&error));
                        cx.notify();
                    }
                    Err(error) => {
                        tracing::warn!(%error, "vault password change task failed");
                        dialog.error = Some(Arc::from(
                            "Failed to change the vault password. See logs for diagnostics.",
                        ));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn clear_inputs(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        for input in [
            &self.current_password_input,
            &self.new_password_input,
            &self.confirm_password_input,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
    }
}

impl Render for ChangeVaultPasswordDialogContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let dialog = cx.entity();
        let device_auth_dialog = dialog.clone();
        let busy = self.pending || self.device_auth_pending;
        let device_auth = device_auth_buttons(
            self.device_auth.as_ref(),
            "wallet-change-vault-password-touch-id",
            self.device_auth_pending,
            self.pending,
            move |method, window, cx| {
                device_auth_dialog.update(cx, |dialog, cx| {
                    dialog.submit_with_device_auth(method, window, cx);
                });
            },
        );
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(password_field(
                "Current password",
                &self.current_password_input,
                busy,
                device_auth,
            ))
            .child(password_field(
                "New password",
                &self.new_password_input,
                busy,
                Vec::new(),
            ))
            .child(password_field(
                "Confirm new password",
                &self.confirm_password_input,
                busy,
                Vec::new(),
            ))
            .when(self.device_auth.is_some(), |this| {
                this.child(
                    app_muted_text(
                        "Enter the new password twice, then approve with an enabled device or the current password.",
                    )
                    .text_xs()
                    .whitespace_normal(),
                )
            })
            .when_some(self.error.as_ref(), |this, error| {
                this.child(
                    app_muted_text(error.to_string())
                        .text_color(rgb(theme::DANGER))
                        .whitespace_normal(),
                )
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("wallet-change-vault-password-cancel", "Cancel")
                            .disabled(self.pending)
                            .on_click(move |_event, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        app_button(
                            "wallet-change-vault-password-submit",
                            if self.pending {
                                "Changing..."
                            } else {
                                "Change password"
                            },
                        )
                        .primary()
                        .disabled(busy)
                        .on_click(move |_event, window, cx| {
                            dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                        }),
                    ),
            )
    }
}

impl WalletRoot {
    pub(in crate::root) fn open_change_vault_password_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !matches!(self.vault_state, VaultState::ViewUnlocked) {
            self.set_vault_error("Unlock the wallet vault before changing its password", cx);
            return;
        }
        ChangeVaultPasswordDialogContent::open(self, window, cx);
    }
}

fn password_field(
    label: &'static str,
    input: &Entity<InputState>,
    disabled: bool,
    device_auth: Vec<InputGroupButton>,
) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text(label))
        .child(masked_input_with_device_auth(input, disabled, device_auth))
}

fn change_vault_password_error_message(error: &VaultError) -> Arc<str> {
    match error {
        VaultError::UnlockFailed => {
            Arc::from("Current password did not unlock the vault. Check it and try again.")
        }
        VaultError::VaultNotFound => Arc::from("Wallet vault storage was not found."),
        _ => Arc::from(format!("Failed to change vault password: {error}")),
    }
}

#[cfg(test)]
mod tests {
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
    fn change_password_device_auth_ignores_closed_dialogs_and_wallet_changes(
        cx: &mut TestAppContext,
    ) {
        const PASSWORD: &str = "public list test password";
        const NEW_PASSWORD: &str = "replacement vault password";
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
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        let store = root.read_with(cx, |root, _| root.vault_store.clone().unwrap());
        let wait_for_change = |dialog: &Entity<ChangeVaultPasswordDialogContent>,
                               cx: &mut gpui::VisualTestContext| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while dialog.read_with(cx, |dialog, _| dialog.pending) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "password change timed out"
                );
                runtime.block_on(async {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                });
                cx.run_until_parked();
            }
        };

        for invalidate in ["cancel", "settings", "generation", "lock"] {
            let content = cx.update(|window, cx| {
                root.update(cx, |root, cx| {
                    ChangeVaultPasswordDialogContent::open(root, window, cx)
                })
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                content.update(cx, |dialog, cx| {
                    dialog
                        .new_password_input
                        .update(cx, |input, cx| input.set_value(NEW_PASSWORD, window, cx));
                    dialog
                        .confirm_password_input
                        .update(cx, |input, cx| input.set_value(NEW_PASSWORD, window, cx));
                });
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            content.update(cx, |dialog, cx| {
                dialog.device_auth_pending = true;
                cx.notify();
            });
            // Model the entity ownership of callbacks retained by the rendered frame.
            let retained = content;
            let retained = cx.update(|window, cx| {
                match invalidate {
                    "cancel" => window.close_dialog(cx),
                    "settings" => {
                        root.update(cx, |root, cx| root.open_settings_from_shortcut(window, cx));
                    }
                    "generation" => {
                        root.update(cx, |root, _| root.advance_active_wallet_generation());
                    }
                    "lock" => root.update(cx, |root, cx| root.lock_vault(window, cx)),
                    _ => unreachable!(),
                }
                // Deliver before another draw can drop the rendered button callbacks.
                retained.update(cx, |dialog, cx| {
                    dialog.finish_device_auth(
                        DeviceAuthMethod::TouchId,
                        DeviceAuthPassword::Password(Zeroizing::new(PASSWORD.into())),
                        window,
                        cx,
                    );
                });
                retained
            });
            wait_for_change(&retained, cx);
            assert!(
                store.unlock_view(PASSWORD).is_ok(),
                "dismissed authentication changed the vault password"
            );
            assert!(store.unlock_view(NEW_PASSWORD).is_err());
            cx.update(WindowExt::close_all_dialogs);
        }

        let content = cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.vault_state = VaultState::ViewUnlocked;
                root.view_session = Some(Arc::new(
                    store.load_view_session(PASSWORD, "preview").unwrap(),
                ));
                ChangeVaultPasswordDialogContent::open(root, window, cx)
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            content.update(cx, |dialog, cx| {
                dialog
                    .new_password_input
                    .update(cx, |input, cx| input.set_value(NEW_PASSWORD, window, cx));
                dialog
                    .confirm_password_input
                    .update(cx, |input, cx| input.set_value(NEW_PASSWORD, window, cx));
                dialog.device_auth_pending = true;
                dialog.finish_device_auth(
                    DeviceAuthMethod::TouchId,
                    DeviceAuthPassword::Password(Zeroizing::new(PASSWORD.into())),
                    window,
                    cx,
                );
            });
        });
        wait_for_change(&content, cx);
        assert!(store.unlock_view(NEW_PASSWORD).is_ok());
        assert!(store.unlock_view(PASSWORD).is_err());
        cx.update(|window, _| window.remove_window());
    }
}
