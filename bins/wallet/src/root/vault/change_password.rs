use std::sync::Arc;

use gpui::{
    AppContext, Context, Entity, Focusable, IntoElement, ParentElement, Render, Styled, Window,
    div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, WindowExt,
    button::ButtonVariants,
    input::{InputEvent, InputState},
};
use ui::controls::{app_button, app_masked_input, app_muted_text, app_strong_text};
use ui::theme;
use wallet_ops::vault::VaultError;
use zeroize::Zeroizing;

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
}

#[derive(Clone, Copy)]
enum ChangeVaultPasswordEnterAction {
    FocusNewPassword,
    FocusConfirmPassword,
    Submit,
}

impl ChangeVaultPasswordDialogContent {
    fn new(root: Entity<WalletRoot>, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
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
        }
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
        if self.pending {
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
        if self.pending {
            return;
        }
        let current_password =
            Zeroizing::new(self.current_password_input.read(cx).value().to_string());
        let new_password = Zeroizing::new(self.new_password_input.read(cx).value().to_string());
        let confirm_password =
            Zeroizing::new(self.confirm_password_input.read(cx).value().to_string());

        if current_password.trim().is_empty() {
            self.error = Some(Arc::from("Enter the current vault password"));
            cx.notify();
            return;
        }
        if new_password.trim().is_empty() {
            self.error = Some(Arc::from("Enter a new vault password"));
            cx.notify();
            return;
        }
        if new_password.as_str() != confirm_password.as_str() {
            self.error = Some(Arc::from("New vault passwords do not match"));
            cx.notify();
            return;
        }
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
                        root.update(cx, WalletRoot::clear_spend_authorization);
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
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(password_field(
                "Current password",
                &self.current_password_input,
                self.pending,
            ))
            .child(password_field(
                "New password",
                &self.new_password_input,
                self.pending,
            ))
            .child(password_field(
                "Confirm new password",
                &self.confirm_password_input,
                self.pending,
            ))
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
                        .disabled(self.pending)
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
        window.close_all_dialogs(cx);
        let root = cx.entity();
        let content = cx.new(|cx| ChangeVaultPasswordDialogContent::new(root, window, cx));
        let focus_content = content.clone();
        let dialog_width =
            (window.viewport_size().width * 0.92).min(CHANGE_VAULT_PASSWORD_DIALOG_WIDTH);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, _cx| {
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .title(app_strong_text("Change vault password"))
                .child(div().w(content_width).child(content.clone()))
        });
        cx.defer_in(window, move |_root, window, cx| {
            focus_content.update(cx, |content, cx| {
                content.focus_current_password(window, cx);
            });
        });
    }
}

fn password_field(label: &'static str, input: &Entity<InputState>, disabled: bool) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text(label))
        .child(app_masked_input(input, disabled))
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
