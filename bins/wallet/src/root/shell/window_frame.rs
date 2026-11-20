#[allow(clippy::wildcard_imports)]
use super::*;

impl Render for WalletRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if self.private_pending_status_dialog_open && !window.has_active_dialog(cx) {
            self.private_pending_status_dialog_open = false;
        }
        let pending_ppoi_validation_toast = self.pending_ppoi_validation_toast.take();
        if pending_ppoi_validation_toast.is_some_and(|(wallet_id, wallet_generation)| {
            ppoi_validation_toast_scope_is_current(
                self.selected_wallet_id.as_deref(),
                self.active_wallet_generation,
                wallet_id.as_ref(),
                wallet_generation,
            )
        }) {
            window.push_notification(
                Notification::success("Outgoing transaction proofs recovered."),
                cx,
            );
        }
        if std::mem::take(&mut self.pending_activity_journal_notice) {
            window.push_notification(
                Notification::warning(
                    "Couldn't update Activity for a recent transaction. The transaction itself \
                     wasn't affected.",
                ),
                cx,
            );
        }
        self.apply_public_broadcaster_error_amount_adjustments(window, cx);
        self.sync_platform_attention_for_window(window);
        self.ensure_prover_cache_build_monitor(cx);
        self.ensure_stealth_accounts(window, cx);
        self.ensure_private_swaps(window, cx);
        if should_apply_background_focus(window.has_active_dialog(cx)) {
            self.focus_vault_input_if_requested(window, cx);
            self.prompt_device_auth_on_unlock_screen_if_requested(window, cx);
            self.focus_utxo_table_if_requested(window, cx);
            self.focus_public_account_search_if_requested(window, cx);
            self.focus_wallet_activity_list_if_requested(window, cx);
        }

        let root = cx.entity();
        if !matches!(self.vault_state, VaultState::ViewUnlocked) {
            // Debug UI fixture: an `update-check:` mode opens its dialog over the first frame.
            #[cfg(debug_assertions)]
            self.open_ui_fixture_update_check(window, cx);
            return self.render_locked_vault_screen(root, window);
        }
        // Debug UI fixture: a `walletconnect-request` mode stages its canned dapp request.
        #[cfg(debug_assertions)]
        self.stage_ui_fixture_walletconnect_request();
        self.open_next_walletconnect_request_dialog_if_idle(window, cx);
        let sidebar_is_narrow = window.viewport_size().width < SIDEBAR_AUTO_COLLAPSE_WIDTH;
        if !sidebar_is_narrow {
            self.sidebar_narrow_expanded = false;
        }
        let sidebar_collapsed = if sidebar_is_narrow {
            !self.sidebar_narrow_expanded
        } else {
            self.sidebar_manually_collapsed
        };
        let dialog_open = window.has_active_dialog(cx);

        div()
            .when(self.settings_editor.is_some(), |this| {
                this.on_action(cx.listener(
                    |this, action: &ui::chain_select::EditChain, window, cx| {
                        this.open_chain_editor(action.chain_id, window, cx);
                    },
                ))
            })
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .text_color(rgb(theme::TEXT))
            .font_family(APP_FONT_FAMILY)
            .font_weight(FontWeight::LIGHT)
            .text_size(APP_TEXT_SIZE)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .flex()
                    .child(self.render_sidebar(root.clone(), sidebar_collapsed, sidebar_is_narrow))
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex_1()
                                    .min_h(px(0.0))
                                    .child(self.render_workspace(&root, window, cx)),
                            ),
                    ),
            )
            .child(self.render_bottom_block(&root, dialog_open, window, cx))
    }
}

pub(super) fn wallet_titlebar_options() -> gpui::TitlebarOptions {
    let mut options = TitleBar::title_bar_options();
    options.title = Some(SharedString::from("RailOxide"));
    options
}

pub(in crate::root) fn render_wallet_window_frame(
    content: gpui::AnyElement,
    window: &Window,
    titlebar_color: u32,
) -> gpui::Div {
    div()
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .text_color(rgb(theme::TEXT))
        .font_family(APP_FONT_FAMILY)
        .font_weight(FontWeight::LIGHT)
        .text_size(APP_TEXT_SIZE)
        .when(should_render_wallet_title_bar(window), |this| {
            this.child(render_wallet_title_bar(titlebar_color))
        })
        .child(div().flex_1().min_w(px(0.0)).min_h(px(0.0)).child(content))
}

fn should_render_wallet_title_bar(window: &Window) -> bool {
    !cfg!(any(target_os = "linux", target_os = "freebsd"))
        || matches!(
            window.window_decorations(),
            gpui::Decorations::Client { .. }
        )
}

fn render_wallet_title_bar(titlebar_color: u32) -> TitleBar {
    TitleBar::new()
        .bg(rgb(titlebar_color))
        .border_color(rgb(titlebar_color))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w(px(0.0))
                .child(img(LOGO_ICON_PATH).size(px(16.0)))
                .child(
                    div()
                        .text_color(rgb(theme::TEXT))
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child("RailOxide"),
                ),
        )
}
