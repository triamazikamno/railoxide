use super::*;

impl WalletRoot {
    pub(in crate::root) fn walletconnect_pending_request_count(&self) -> usize {
        self.walletconnect.pending_requests.len()
    }

    pub(in crate::root) fn walletconnect_session_count(&self) -> usize {
        self.walletconnect
            .sessions
            .iter()
            .filter(|session| walletconnect_session_visible_in_management(session))
            .count()
    }

    pub(in crate::root) fn walletconnect_account_session_count(
        &self,
        public_account_uuid: &str,
    ) -> usize {
        self.walletconnect
            .sessions
            .iter()
            .filter(|session| {
                session.selected_public_account_uuid == public_account_uuid
                    && walletconnect_session_visible_in_management(session)
            })
            .count()
    }

    pub(in crate::root) fn render_walletconnect_toolbar_button(
        &self,
        root: &Entity<Self>,
    ) -> impl IntoElement {
        let trigger_root = root.clone();
        let session_count = self.walletconnect_session_count();
        let pending_count = self.walletconnect_pending_request_count();
        let disabled = self.view_session.is_none()
            || self.vault_store.is_none()
            || (pending_count == 0 && session_count == 0 && !self.has_active_public_accounts());
        app_button_base("wallet-public-walletconnect-trigger")
            .text()
            .size(px(32.0))
            .when(!disabled, gpui::Styled::cursor_pointer)
            .disabled(disabled)
            .accessibility_label(if pending_count > 0 {
                "Review dapp request".to_owned()
            } else if session_count > 0 {
                format!("WalletConnect sessions · {session_count}")
            } else {
                "Connect dapp".to_owned()
            })
            .tooltip(if pending_count > 0 {
                "Review dapp request".to_owned()
            } else if session_count > 0 {
                format!("WalletConnect sessions · {session_count}")
            } else {
                "Connect dapp".to_owned()
            })
            .child(walletconnect_logo_with_badges(
                px(24.0),
                session_count > 0,
                pending_count,
            ))
            .on_click(move |_event, window, cx| {
                trigger_root.update(cx, |root, cx| {
                    if root.walletconnect_pending_request_count() > 0 {
                        root.open_walletconnect_pending_request_dialog(window, cx);
                    } else if root.walletconnect_session_count() > 0 {
                        root.open_walletconnect_sessions_dialog(window, cx);
                    } else {
                        root.open_walletconnect_connection_dialog_for_default_account(window, cx);
                    }
                });
            })
    }
}
