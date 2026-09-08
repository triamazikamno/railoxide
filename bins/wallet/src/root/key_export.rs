use std::sync::Arc;

use gpui::{
    AppContext, Context, Entity, Focusable, IntoElement, ParentElement, Pixels, Render,
    SharedString, Styled, Window, div, prelude::FluentBuilder, px, rgb,
};
use gpui_component::{Sizable, WindowExt, alert::Alert, button::ButtonVariants, input::InputEvent};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button, app_masked_input, app_muted_text, app_strong_text};
use ui::theme::{self, APP_MONO_FONT_FAMILY};
use wallet_ops::vault::{
    VaultError, WalletMetadataBundle, WalletSoftwareContextKind, WalletSource,
};
use zeroize::Zeroizing;

use super::{
    APP_TEXT_SIZE, WalletRoot, dialog_max_height, new_masked_input, secondary_dialog_content_width,
    vault_error_kind,
};

pub(in crate::root) const WALLET_EXPORT_MENU_LABEL: &str = "Export keys";
const KEY_EXPORT_DIALOG_WIDTH: Pixels = px(560.0);
const KEY_EXPORT_PASSWORD_DIALOG_WIDTH: Pixels = px(420.0);
const MASKED_VALUE: &str = "********************************";

#[derive(Default)]
pub(super) struct KeyExportState {
    wallet_id: Option<Arc<str>>,
    wallet_label: Option<Arc<str>>,
    wallet_source: Option<WalletSource>,
    wallet_context_kind: Option<WalletSoftwareContextKind>,
    base_profile_uuid: Option<Arc<str>>,
    mnemonic: Option<Zeroizing<String>>,
    shareable_viewing_key: Option<Zeroizing<String>>,
}

impl KeyExportState {
    fn begin(&mut self, wallet: &WalletMetadataBundle) {
        self.clear_values();
        self.wallet_id = Some(Arc::from(wallet.wallet_uuid.clone()));
        self.wallet_label = Some(Arc::from(wallet.label.clone()));
        self.wallet_source = Some(wallet.source);
        self.wallet_context_kind = wallet.software_context.as_ref().map(|context| context.kind);
        self.base_profile_uuid = wallet
            .software_context
            .as_ref()
            .map(|context| Arc::from(context.base_profile_uuid.clone()));
    }

    fn clear(&mut self) {
        self.clear_values();
        self.wallet_id = None;
        self.wallet_label = None;
        self.wallet_source = None;
        self.wallet_context_kind = None;
        self.base_profile_uuid = None;
    }

    fn clear_values(&mut self) {
        self.mnemonic = None;
        self.shareable_viewing_key = None;
    }
}

#[derive(Clone, Copy)]
pub(in crate::root) enum KeyExportSecretKind {
    Mnemonic,
    ShareableViewingKey,
}

struct KeyExportPasswordDialogContent {
    root: Entity<WalletRoot>,
    kind: KeyExportSecretKind,
    password_input: Entity<gpui_component::input::InputState>,
    error: Option<Arc<str>>,
}

impl KeyExportPasswordDialogContent {
    fn new(
        root: Entity<WalletRoot>,
        kind: KeyExportSecretKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let password_input = new_masked_input(window, cx, "vault password");
        cx.subscribe_in(
            &password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                InputEvent::Change => {
                    this.error = None;
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        Self {
            root,
            kind,
            password_input,
            error: None,
        }
    }

    fn focus_password(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.password_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let password = Zeroizing::new(self.password_input.read(cx).value().to_string());
        if password.trim().is_empty() {
            self.error = Some(Arc::from(key_export_password_empty_message(self.kind)));
            cx.notify();
            return;
        }

        let kind = self.kind;
        let result = self.root.update(cx, |root, cx| {
            root.reveal_key_export_secret(kind, &password, cx)
        });
        match result {
            Ok(()) => {
                self.password_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                window.close_dialog(cx);
            }
            Err(message) => {
                self.error = Some(message);
                cx.notify();
            }
        }
    }
}

impl Render for KeyExportPasswordDialogContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let dialog = cx.entity();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(app_muted_text(key_export_password_prompt_copy(self.kind)).whitespace_normal())
            .child(app_masked_input(&self.password_input, false))
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
                        app_button("wallet-key-export-password-cancel", "Cancel")
                            .flex_none()
                            .on_click(move |_event, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        app_button(key_export_password_submit_button_id(self.kind), "Reveal")
                            .primary()
                            .flex_none()
                            .on_click(move |_event, window, cx| {
                                dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                            }),
                    ),
            )
    }
}

impl WalletRoot {
    pub(super) fn open_key_export_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(wallet) = self.selected_key_export_wallet().cloned() else {
            self.set_vault_error("Select a private wallet before exporting keys", cx);
            return;
        };
        window.close_all_dialogs(cx);
        self.key_export.begin(&wallet);

        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(KEY_EXPORT_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(WALLET_EXPORT_MENU_LABEL))
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.clear_key_export_dialog_state(window, cx);
                    });
                })
                .child(
                    content_root
                        .read(cx)
                        .render_key_export_dialog_content(&content_root, content_width),
                )
        });
    }

    pub(in crate::root) fn clear_key_export_dialog_state(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) {
        self.key_export.clear();
    }

    fn open_key_export_password_dialog(
        kind: KeyExportSecretKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let root = cx.entity();
        let content_root = root;
        let content =
            cx.new(|cx| KeyExportPasswordDialogContent::new(content_root, kind, window, cx));
        let focus_content = content.clone();
        let dialog_width =
            (window.viewport_size().width * 0.92).min(KEY_EXPORT_PASSWORD_DIALOG_WIDTH);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, _cx| {
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .title(app_strong_text(key_export_password_dialog_title(kind)))
                .child(div().w(content_width).child(content.clone()))
        });
        cx.defer_in(window, move |_root, window, cx| {
            focus_content.update(cx, |content, cx| content.focus_password(window, cx));
        });
    }

    fn selected_key_export_wallet(&self) -> Option<&WalletMetadataBundle> {
        let selected_wallet_id = self.selected_wallet_id.as_deref()?;
        self.wallet_metadata
            .iter()
            .find(|wallet| wallet.wallet_uuid == selected_wallet_id)
    }

    fn reveal_key_export_secret(
        &mut self,
        kind: KeyExportSecretKind,
        password: &Zeroizing<String>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), Arc<str>> {
        match kind {
            KeyExportSecretKind::Mnemonic => self.reveal_key_export_mnemonic(password, cx),
            KeyExportSecretKind::ShareableViewingKey => {
                self.reveal_key_export_shareable_viewing_key(password, cx)
            }
        }
    }

    fn reveal_key_export_mnemonic(
        &mut self,
        password: &Zeroizing<String>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), Arc<str>> {
        if matches!(self.key_export.wallet_source, Some(source) if !key_export_mnemonic_available(source))
        {
            return Err(Arc::from(
                "Mnemonic seed export is unavailable for hardware-derived wallets.",
            ));
        }
        let Some(wallet_id) = self.key_export.wallet_id.clone() else {
            return Err(Arc::from("Selected wallet is unavailable."));
        };
        let Some(store) = self.vault_store.clone() else {
            return Err(Arc::from("Wallet vault storage is unavailable."));
        };

        match store.export_wallet_mnemonic_for_context(password, wallet_id.as_ref()) {
            Ok(mnemonic) => {
                self.key_export.mnemonic = Some(mnemonic);
                cx.notify();
                Ok(())
            }
            Err(error) => {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    secret = "mnemonic",
                    "desktop key export failed"
                );
                Err(key_export_error_message(
                    &error,
                    KeyExportSecretKind::Mnemonic,
                ))
            }
        }
    }

    fn reveal_key_export_shareable_viewing_key(
        &mut self,
        password: &Zeroizing<String>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), Arc<str>> {
        let Some(wallet_id) = self.key_export.wallet_id.clone() else {
            return Err(Arc::from("Selected wallet is unavailable."));
        };
        let Some(store) = self.vault_store.clone() else {
            return Err(Arc::from("Wallet vault storage is unavailable."));
        };

        let result = match self.key_export.wallet_source {
            Some(source) if source.is_hardware_derived() => store
                .export_hardware_wallet_shareable_viewing_key(
                    password,
                    wallet_id.as_ref(),
                    self.view_session.as_deref(),
                ),
            _ => {
                store.export_wallet_shareable_viewing_key_for_context(password, wallet_id.as_ref())
            }
        };

        match result {
            Ok(shareable_viewing_key) => {
                self.key_export.shareable_viewing_key = Some(shareable_viewing_key);
                cx.notify();
                Ok(())
            }
            Err(error) => {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    secret = "shareable_viewing_key",
                    "desktop key export failed"
                );
                Err(key_export_error_message(
                    &error,
                    KeyExportSecretKind::ShareableViewingKey,
                ))
            }
        }
    }

    fn render_key_export_dialog_content(
        &self,
        root: &Entity<Self>,
        content_width: Pixels,
    ) -> gpui::Div {
        let wallet_label = self
            .key_export
            .wallet_label
            .as_ref()
            .map_or_else(|| "selected wallet".to_owned(), ToString::to_string);
        let source = self.key_export.wallet_source.unwrap_or_default();

        div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_4()
            .child(Alert::warning("wallet-key-export-warning", key_export_warning_copy()).small())
            .children(
                key_export_context_warning(
                    self.key_export.wallet_context_kind,
                    self.key_export.base_profile_uuid.as_deref(),
                )
                .map(|warning| {
                    Alert::warning("wallet-key-export-context-warning", warning).small()
                }),
            )
            .child(app_muted_text(format!("Selected wallet: {wallet_label}")).whitespace_normal())
            .child(self.render_mnemonic_export_row(root, source))
            .child(self.render_shareable_viewing_key_export_row(root, source))
    }

    fn render_mnemonic_export_row(&self, root: &Entity<Self>, source: WalletSource) -> gpui::Div {
        if !key_export_mnemonic_available(source) {
            return render_key_export_unavailable_row(
                "Mnemonic seed",
                "Unavailable for hardware-derived wallets. This app does not store a Railgun mnemonic, synthetic entropy, hardware output, or hardware passphrase for this wallet.",
            );
        }
        render_key_export_reveal_row(
            root,
            KeyExportSecretKind::Mnemonic,
            "Mnemonic seed",
            "Show mnemonic seed",
            "wallet-key-export-mnemonic-copy",
            self.key_export.mnemonic.as_ref(),
        )
    }

    fn render_shareable_viewing_key_export_row(
        &self,
        root: &Entity<Self>,
        source: WalletSource,
    ) -> gpui::Div {
        let help = if source.is_hardware_derived() {
            "Requires the matching hardware-derived Private wallet view session to be active. Unlock/select the matching hardware wallet first."
        } else {
            "This key gives full viewing access to your entire transaction history, including future activity. It cannot be revoked."
        };
        render_key_export_reveal_row(
            root,
            KeyExportSecretKind::ShareableViewingKey,
            "Shareable view-only key",
            "Show view-only key",
            "wallet-key-export-view-only-copy",
            self.key_export.shareable_viewing_key.as_ref(),
        )
        .child(app_muted_text(help).whitespace_normal())
    }
}

fn render_key_export_reveal_row(
    root: &Entity<WalletRoot>,
    kind: KeyExportSecretKind,
    label: &'static str,
    button_label: &'static str,
    copy_button_id: &'static str,
    value: Option<&Zeroizing<String>>,
) -> gpui::Div {
    let revealed = key_export_copy_available(value.is_some());
    let reveal_root = root.clone();
    let display_value = value.map_or_else(
        || MASKED_VALUE.to_owned(),
        |value| value.as_str().to_owned(),
    );
    let row = div()
        .flex()
        .flex_col()
        .gap_2()
        .p(px(12.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .child(app_strong_text(label))
        .child(render_key_export_value_field(
            display_value,
            revealed,
            copy_button_id,
        ));

    if revealed {
        row
    } else {
        row.child(
            div().flex().items_center().justify_end().gap_2().child(
                app_button(key_export_reveal_button_id(kind), button_label)
                    .primary()
                    .small()
                    .flex_none()
                    .on_click(move |_event, window, cx| {
                        reveal_root.update(cx, |_root, cx| {
                            WalletRoot::open_key_export_password_dialog(kind, window, cx);
                        });
                    }),
            ),
        )
    }
}

fn render_key_export_unavailable_row(label: &'static str, message: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p(px(12.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .child(app_strong_text(label))
        .child(render_key_export_value_field(
            "Unavailable".to_owned(),
            false,
            "wallet-key-export-unavailable-copy",
        ))
        .child(app_muted_text(message).whitespace_normal())
}

fn render_key_export_value_field(
    value: String,
    revealed: bool,
    copy_button_id: &'static str,
) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .p(px(8.0))
                .rounded_sm()
                .bg(rgb(theme::BACKGROUND))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .font_family(APP_MONO_FONT_FAMILY)
                .text_size(APP_TEXT_SIZE)
                .text_color(rgb(if revealed {
                    theme::TEXT
                } else {
                    theme::TEXT_MUTED
                }))
                .whitespace_normal()
                .child(SharedString::from(value.clone())),
        )
        .children(revealed.then(|| clipboard_with_toast(copy_button_id, value)))
}

pub(in crate::root) const fn key_export_mnemonic_available(source: WalletSource) -> bool {
    !source.is_hardware_derived()
}

pub(in crate::root) const fn key_export_context_warning(
    context_kind: Option<WalletSoftwareContextKind>,
    base_profile_uuid: Option<&str>,
) -> Option<&'static str> {
    match key_export_context_warning_predicates(context_kind, base_profile_uuid) {
        Some(_) => Some(
            "This mnemonic alone cannot restore this wallet. The exact mnemonic passphrase is also required and is not retained by the app.",
        ),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) struct KeyExportContextWarningPredicates {
    pub(in crate::root) mnemonic_alone_is_insufficient: bool,
    pub(in crate::root) passphrase_is_external: bool,
    pub(in crate::root) passphrase_is_not_retained: bool,
}

pub(in crate::root) const fn key_export_context_warning_predicates(
    context_kind: Option<WalletSoftwareContextKind>,
    base_profile_uuid: Option<&str>,
) -> Option<KeyExportContextWarningPredicates> {
    match context_kind {
        Some(WalletSoftwareContextKind::Passphrase) if base_profile_uuid.is_some() => {
            Some(KeyExportContextWarningPredicates {
                mnemonic_alone_is_insufficient: true,
                passphrase_is_external: true,
                passphrase_is_not_retained: true,
            })
        }
        _ => None,
    }
}

pub(in crate::root) const fn key_export_copy_available(revealed: bool) -> bool {
    revealed
}

pub(in crate::root) const fn key_export_reveal_button_id(
    kind: KeyExportSecretKind,
) -> &'static str {
    match kind {
        KeyExportSecretKind::Mnemonic => "wallet-key-export-show-mnemonic",
        KeyExportSecretKind::ShareableViewingKey => "wallet-key-export-show-view-only",
    }
}

pub(in crate::root) const fn key_export_password_submit_button_id(
    kind: KeyExportSecretKind,
) -> &'static str {
    match kind {
        KeyExportSecretKind::Mnemonic => "wallet-key-export-submit-mnemonic",
        KeyExportSecretKind::ShareableViewingKey => "wallet-key-export-submit-view-only",
    }
}

pub(in crate::root) const fn key_export_password_dialog_title(
    kind: KeyExportSecretKind,
) -> &'static str {
    match kind {
        KeyExportSecretKind::Mnemonic => "Reveal mnemonic seed",
        KeyExportSecretKind::ShareableViewingKey => "Reveal view-only key",
    }
}

const fn key_export_password_prompt_copy(kind: KeyExportSecretKind) -> &'static str {
    match kind {
        KeyExportSecretKind::Mnemonic => {
            "Enter the vault password to reveal only the mnemonic seed for this wallet."
        }
        KeyExportSecretKind::ShareableViewingKey => {
            "Enter the vault password to reveal only the shareable view-only key for this wallet."
        }
    }
}

const fn key_export_password_empty_message(kind: KeyExportSecretKind) -> &'static str {
    match kind {
        KeyExportSecretKind::Mnemonic => "Enter the vault password to reveal the mnemonic seed.",
        KeyExportSecretKind::ShareableViewingKey => {
            "Enter the vault password to reveal the view-only key."
        }
    }
}

pub(in crate::root) fn key_export_error_message(
    error: &VaultError,
    kind: KeyExportSecretKind,
) -> Arc<str> {
    match error {
        VaultError::UnlockFailed => "Password did not unlock the vault. Check it and try again.".into(),
        VaultError::WalletMnemonicUnavailable => {
            "Mnemonic seed export is unavailable for hardware-derived wallets.".into()
        }
        VaultError::HardwareWalletViewRequiresDevice => match kind {
            KeyExportSecretKind::Mnemonic => {
                "Mnemonic seed export is unavailable for hardware-derived wallets.".into()
            }
            KeyExportSecretKind::ShareableViewingKey => {
                "Unlock the matching hardware wallet before exporting the view-only key.".into()
            }
        },
        VaultError::HardwareWalletIdentityMismatch => {
            "The active hardware wallet does not match this wallet. Unlock the matching device profile and account, then try again.".into()
        }
        VaultError::UnsupportedHardwareCustodyBackend(_) => {
            "This hardware wallet custody backend is not supported by this app version.".into()
        }
        _ => "Key export failed. See logs for non-sensitive diagnostics.".into(),
    }
}

const fn key_export_warning_copy() -> &'static str {
    "Revealed keys can compromise wallet funds, balances, and transaction history. Clipboard contents may be visible outside the wallet app after copying. Only reveal keys on a trusted device and close this dialog when finished."
}
