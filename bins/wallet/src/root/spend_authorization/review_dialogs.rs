#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    /// Counts a spend authorization dialog toward platform attention for as long as the
    /// returned lease lives. The dialog builder holds it, so any way of closing releases it.
    pub(super) fn spend_authorization_attention_lease(
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> Rc<()> {
        let lease = Rc::new(());
        let open = Rc::downgrade(&lease);
        cx.defer_in(window, move |root, window, _cx| {
            root.spend_authorization_attention.push(open);
            root.sync_platform_attention_for_window(window);
        });
        lease
    }

    pub(super) fn open_spend_authorization_dialog(
        &self,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_spend_authorization_dialog_with_review(intent, summary, None, window, cx);
    }

    pub(in crate::root) fn open_prepared_spend_review(
        &self,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        intent.private_attention(
            "Review in the desktop app",
            "Review the updated transaction terms before signing.",
        );
        self.open_spend_authorization_dialog_with_review(
            intent,
            summary,
            Some((self.current_spend_authorization_scope(), authorization)),
            window,
            cx,
        );
    }

    pub(super) fn open_spend_authorization_dialog_with_review(
        &self,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        review_authorization: Option<(SpendAuthorizationScope, DesktopPrivateSpendAuthorization)>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<SpendAuthorizationDialogContent> {
        let root = cx.entity();
        let initial_lifetime = self.spend_authorization_lifetime;
        let dialog_title = summary.title.to_string();
        let title_network = summary.title_network;
        let device_auth = self.device_auth_prompt_cached();
        let lease = Rc::new(Cell::new(true));
        let identity = Rc::downgrade(&lease);
        let attention = Self::spend_authorization_attention_lease(window, cx);
        let content = cx.new(|cx| {
            let mut content = SpendAuthorizationDialogContent::new(
                root,
                intent,
                summary,
                initial_lifetime,
                identity,
                window,
                cx,
            );
            content.review_authorization = review_authorization;
            content.device_auth = device_auth;
            content
        });
        let focus_content = content.clone();
        let dialog_content = content.clone();
        window.open_wallet_dialog(cx, move |dialog, window, cx| {
            let dialog_width =
                (window.viewport_size().width * 0.92).min(SPEND_AUTHORIZATION_DIALOG_WIDTH);
            let content_width = secondary_dialog_content_width(dialog_width);
            let close_content = dialog_content.clone();
            let identity = Rc::downgrade(&lease);
            let _ = &attention;
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(dialog_max_height(window))
                .title(spend_authorization_title(&dialog_title, title_network))
                .on_close(move |_event, _window, cx| {
                    if let Some(open) = identity.upgrade() {
                        open.set(false);
                    }
                    close_content.update(cx, SpendAuthorizationDialogContent::cancel);
                })
                .child(spend_authorization_dialog_body(
                    dialog_content.clone(),
                    content_width,
                    window,
                ))
                // The footer stays in view while the content scrolls.
                .footer(
                    dialog_content
                        .read(cx)
                        .render_footer(&dialog_content)
                        .w(content_width),
                )
        });
        cx.defer_in(window, move |_root, window, cx| {
            focus_content.update(cx, |content, cx| content.focus_password(window, cx));
        });
        content
    }
}
