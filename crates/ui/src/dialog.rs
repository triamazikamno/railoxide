//! Wallet dialog openers.

use gpui::{App, Window};
use gpui_component::{
    WindowExt,
    dialog::{AlertDialog, Dialog, DialogEntrance},
};

/// Opens dialogs without an exit transition: dismissal guards need the builder dropped on close.
pub trait WalletDialogExt {
    /// [`WindowExt::open_dialog`] with [`DialogEntrance::None`].
    fn open_wallet_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static;

    /// [`WindowExt::open_alert_dialog`] with [`DialogEntrance::None`].
    fn open_wallet_alert_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(AlertDialog, &mut Window, &mut App) -> AlertDialog + 'static;
}

impl WalletDialogExt for Window {
    #[allow(clippy::disallowed_methods, reason = "this is the sanctioned wrapper")]
    fn open_wallet_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(Dialog, &mut Self, &mut App) -> Dialog + 'static,
    {
        self.open_dialog(cx, move |dialog, window, cx| {
            build(dialog.entrance(DialogEntrance::None), window, cx)
        });
    }

    #[allow(clippy::disallowed_methods, reason = "this is the sanctioned wrapper")]
    fn open_wallet_alert_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(AlertDialog, &mut Self, &mut App) -> AlertDialog + 'static,
    {
        self.open_alert_dialog(cx, move |dialog, window, cx| {
            build(dialog.entrance(DialogEntrance::None), window, cx)
        });
    }
}
