use std::sync::Arc;

use gpui::{Context, ParentElement as _, Styled as _, Window, div, relative};
use gpui_component::tooltip::Tooltip;
use ui::theme::APP_TEXT_LINE_HEIGHT;
use wallet_ops::gateway::{
    GatewayUnlockAttempt, GatewayUnlockCommand, GatewayUnlockPhase, GatewayUnlockRequest,
};

use super::super::{VaultState, WalletRoot};

pub(super) const UNLOCK_TRUST_SUMMARY: &str =
    "Trusts this extension with your vault password and mnemonic passphrase.";
const UNLOCK_TRUST_TOOLTIP: &str = "Enter your vault password and mnemonic passphrase in this browser extension to unlock the desktop wallet. A compromised browser or extension could steal both. Signing and transactions still require desktop approval.";

pub(super) fn unlock_trust_tooltip(window: &Window) -> Tooltip {
    let width =
        (window.viewport_size().width - window.rem_size() * 3.0).min(window.rem_size() * 20.0);
    Tooltip::element(move |_, _| {
        div()
            .max_w(width)
            .whitespace_normal()
            .line_height(relative(APP_TEXT_LINE_HEIGHT))
            .child(UNLOCK_TRUST_TOOLTIP)
    })
}

impl WalletRoot {
    pub(super) fn apply_gateway_unlock(
        &mut self,
        request: &GatewayUnlockRequest,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((command, guard)) = request.take() else {
            return;
        };
        if matches!(command, GatewayUnlockCommand::Password { .. }) {
            if !matches!(self.vault_state, VaultState::UnlockVault)
                || self.unlock_in_progress
                || self.manage_wallets.deleting_wallet_id.is_some()
            {
                guard.finish(GatewayUnlockPhase::Unavailable);
                return;
            }
        } else if !matches!(self.vault_state, VaultState::PendingSoftwareProfileOpen)
            || !self
                .remote_unlock_attempt()
                .is_some_and(|attempt| Arc::ptr_eq(&attempt, &guard.attempt()))
        {
            return;
        }
        match command {
            GatewayUnlockCommand::Password { password } => {
                self.unlock_vault_with_password(password.into_value(), Some(guard), window, cx);
            }
            GatewayUnlockCommand::Passphrase { passphrase } => self
                .submit_pending_software_passphrase_with_unlock(
                    passphrase.into_value(),
                    Some(guard),
                    window,
                    cx,
                ),
            GatewayUnlockCommand::Standard => {
                self.continue_pending_without_passphrase_with_unlock(
                    false,
                    Some(guard),
                    window,
                    cx,
                );
            }
            GatewayUnlockCommand::Retry => {
                if let Some(pending) = &mut self.pending_software_profile_open {
                    pending.retry();
                    self.vault_error = None;
                    guard.finish(GatewayUnlockPhase::Passphrase);
                    cx.notify();
                }
            }
            GatewayUnlockCommand::Desktop => {
                if !guard
                    .attempt()
                    .finish_if_current(GatewayUnlockPhase::Desktop)
                {
                    return;
                }
                self.gateway.unlock.borrow_mut().remote = None;
                window.activate_window();
            }
            GatewayUnlockCommand::Cancel => {}
        }
        cx.notify();
    }

    pub(in crate::root) fn remote_unlock_attempt(&self) -> Option<Arc<GatewayUnlockAttempt>> {
        self.gateway.unlock.borrow().remote.clone()
    }

    pub(in crate::root) fn set_remote_unlock_attempt(
        &self,
        attempt: Option<Arc<GatewayUnlockAttempt>>,
    ) {
        self.gateway.unlock.borrow_mut().remote = attempt;
    }

    /// An explicit desktop action takes ownership of the existing native flow.
    pub(in crate::root) fn take_over_remote_unlock(&self) {
        if let Some(remote) = self.gateway.unlock.borrow_mut().remote.take() {
            remote.cancel();
        }
    }

    pub(in crate::root) fn reconcile_remote_unlock(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .remote_unlock_attempt()
            .is_none_or(|attempt| attempt.is_current())
        {
            return;
        }
        if matches!(self.vault_state, VaultState::PendingSoftwareProfileOpen) {
            self.abandon_pending_software_profile_open(window, cx);
        } else if matches!(self.vault_state, VaultState::SwitchingWallet) {
            self.abandon_wallet_replacement_installation(window, cx);
        } else {
            self.retire_gateway_unlock();
        }
    }
}
