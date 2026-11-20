#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn render_workspace(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let active_content = self.render_active_content(root, window, cx);
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .child(active_content)
    }

    fn render_active_content(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::AnyElement {
        match self.active_activity {
            Activity::Wallet => self.render_wallet_view(root, window, cx).into_any_element(),
            Activity::Broadcaster => self.render_broadcaster_view(root).into_any_element(),
            Activity::AddressBook => self.render_address_book_view(root),
            Activity::Proposals => self
                .render_governance_workspace(root, window, cx)
                .into_any_element(),
            Activity::Settings => self.render_settings_view(root).into_any_element(),
        }
    }

    fn render_settings_view(&self, root: &Entity<Self>) -> impl IntoElement {
        let content = if let Some(editor) = self.settings_editor.as_ref() {
            div().size_full().child(editor.clone()).into_any_element()
        } else {
            let open_root = root.clone();
            div()
                .p(px(24.0))
                .flex()
                .flex_col()
                .gap_3()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(
                    self.settings_error.as_ref().map_or_else(
                        || "Settings are unavailable".to_string(),
                        ToString::to_string,
                    ),
                ))
                .child(data_folder_row(
                    &data_folder_path(&self.options.db_path),
                    None,
                    true,
                    self.data_folder_open_error.clone(),
                    move |_window, cx| {
                        open_root.update(cx, Self::open_settings_data_folder);
                    },
                ))
                .into_any_element()
        };
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .bg(rgb(theme::SURFACE))
            .p(px(16.0))
            .flex()
            .flex_col()
            .gap_3()
            .child(div().flex_1().min_h(px(0.0)).child(content))
    }

    pub(in crate::root) fn render_wallet_view(
        &self,
        root: &Entity<Self>,
        window: &Window,
        cx: &gpui::App,
    ) -> impl IntoElement {
        let wallet_focus = self.wallet_focus.clone();
        div()
            .key_context(WALLET_WORKSPACE_KEY_CONTEXT)
            .track_focus(&self.wallet_focus)
            // Clicking empty wallet space claims focus only when it is outside the view,
            // so an inner input keeps focus.
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                if wallet_focus.contains_focused(window, cx) {
                    window.prevent_default();
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &NextWalletTab, _, cx| {
                    root.update(cx, |root, cx| root.cycle_wallet_tab(true, cx));
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &PreviousWalletTab, _, cx| {
                    root.update(cx, |root, cx| root.cycle_wallet_tab(false, cx));
                }
            })
            // SelectState has no public open method, so focus the trigger and send it the
            // key that opens it. The dispatch is deferred and targets the trigger focused here.
            .on_action({
                let root = root.clone();
                move |_: &OpenWalletSelector, window, cx| {
                    let select = root.read(cx).wallet_select.clone();
                    select.update(cx, |select, cx| select.focus(window, cx));
                    window.dispatch_action(Box::new(gpui_kit::base::actions::SelectDown), cx);
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &OpenChainSelector, window, cx| {
                    let select = root.read(cx).chain_select.clone();
                    select.update(cx, |select, cx| select.focus(window, cx));
                    window.dispatch_action(Box::new(gpui_kit::base::actions::SelectDown), cx);
                }
            })
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .child(self.render_wallet_header(root))
            .child(self.render_wallet_tabs(root, window, cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .p(px(12.0))
                    .child(self.render_wallet_content(root, window, cx)),
            )
    }
}
