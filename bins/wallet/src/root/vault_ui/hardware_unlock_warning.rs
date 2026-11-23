#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    /// The hardware unlock dialog's content. Once the device that opened an account warns
    /// about it, the warning is the dialog's last state, and its Continue installs the account.
    pub(in crate::root) fn render_hardware_profile_unlock_dialog_body(
        &self,
        root: &Entity<Self>,
        content_width: gpui::Pixels,
    ) -> gpui::Div {
        match self.hardware_profile_unlock.unlock_warning.as_ref() {
            Some(pending) => render_hardware_unlock_warning(root, pending.warning).w(content_width),
            None => self.render_hardware_profile_unlock_dialog_content(root, content_width),
        }
    }
}

fn render_hardware_unlock_warning(
    root: &Entity<WalletRoot>,
    warning: HardwareUnlockWarning,
) -> gpui::Div {
    let continue_root = root.clone();
    let (title, paragraphs) = hardware_unlock_warning_copy(warning);
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .w_full()
                .p(px(12.0))
                .flex()
                .flex_col()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::WARNING))
                .bg(rgb_with_alpha(theme::SURFACE, 0.72))
                .child(
                    app_strong_text(title)
                        .text_color(rgb(theme::WARNING))
                        .whitespace_normal(),
                )
                .children(
                    paragraphs
                        .into_iter()
                        .map(|paragraph| ui::controls::app_text(paragraph).whitespace_normal()),
                ),
        )
        .child(
            app_button("hardware-unlock-warning-continue", "Continue")
                .primary()
                .w_full()
                .on_click(move |_event, window, cx| {
                    continue_root.update(cx, |root, cx| {
                        root.continue_hardware_unlock_warning(window, cx);
                    });
                }),
        )
}

fn hardware_unlock_warning_copy(warning: HardwareUnlockWarning) -> (&'static str, Vec<String>) {
    match warning {
        HardwareUnlockWarning::LedgerDivergentSecret(version) => (
            "Don't update the Ledger Ethereum app yet",
            vec![
                format!(
                    "This wallet depends on how Ledger Ethereum app {version} derives keys. Every other version derives different ones."
                ),
                "If you update the app, this wallet stops opening and its funds can't be spent. There is no backup. Before you update, move your funds to another wallet.".to_owned(),
            ],
        ),
        HardwareUnlockWarning::LedgerLeaksWithoutConfirmation(version) => (
            "This Ledger app can leak your wallet keys",
            vec![
                format!(
                    "Ledger Ethereum app {version} hands this wallet's keys to any software that asks, without a prompt on the device. That includes programs on any computer you plug this Ledger into and websites you've allowed to use it."
                ),
                "Update the Ethereum app to 1.24.0 or newer. The wallet stays the same. Updating stops future leaks but can't undo one that already happened.".to_owned(),
                "If this Ledger has been connected to a computer or website you don't trust with the Ethereum app open, or it can't install a newer version (the original Nano S can't), move your funds to a wallet that doesn't come from this Ledger. Another account on this Ledger doesn't help, because all its accounts come from the same device secret.".to_owned(),
            ],
        ),
    }
}
