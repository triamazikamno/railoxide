#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(super) fn render_wallet_tabs(
        &self,
        root: &Entity<Self>,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        let selected_index = WalletTab::ALL
            .iter()
            .position(|tab| *tab == self.active_wallet_tab)
            .unwrap_or(0);
        let tab_root = root.clone();
        let pending_walletconnect_requests = self.walletconnect_pending_request_count();

        TabBar::new("wallet-tabs")
            .underline()
            .w_full()
            .flex_none()
            .px(px(14.0))
            .selected_index(selected_index)
            .when_some(self.render_wallet_tab_hint(window, cx), |tabs, hint| {
                tabs.suffix(hint)
            })
            .on_click(move |index, _window, cx| {
                let Some(tab) = WalletTab::ALL.get(*index).copied() else {
                    return;
                };
                tab_root.update(cx, |root, cx| {
                    root.select_wallet_tab(tab, cx);
                });
            })
            .children(WalletTab::ALL.into_iter().map(|tab| {
                let unavailable = tab.needs_railgun() && !self.selected_chain_has_railgun();
                let icon = Icon::empty()
                    .path(tab.icon_path())
                    .with_size(px(18.0))
                    .text_color(rgb(theme::TEXT));
                let unread = tab == WalletTab::Activity
                    && self.active_wallet_tab != WalletTab::Activity
                    && self.wallet_activity.unread();
                Tab::new()
                    .disabled(unavailable)
                    .when(unavailable, |tab| {
                        let reason = self.railgun_unavailable_reason();
                        tab.tooltip(move |window, cx| Tooltip::new(reason).build(window, cx))
                    })
                    .when(unread, |tab| {
                        tab.tooltip(|window, cx| {
                            Tooltip::new("Funds received since you last opened Activity")
                                .build(window, cx)
                        })
                    })
                    .min_w(px(92.0))
                    .label(tab.label())
                    .map(|tab| {
                        if unread {
                            tab.prefix(
                                div()
                                    .debug_selector(|| "wallet-tab-activity-unread".to_owned())
                                    .child(
                                        Badge::new().dot().color(rgb(theme::PRIMARY)).child(icon),
                                    ),
                            )
                        } else {
                            tab.prefix(icon)
                        }
                    })
                    .when(
                        tab == WalletTab::Public
                            && self.active_wallet_tab != WalletTab::Public
                            && pending_walletconnect_requests > 0,
                        |tab| {
                            tab.suffix(walletconnect_tab_attention_badge(
                                pending_walletconnect_requests,
                            ))
                        },
                    )
            }))
    }

    fn render_wallet_tab_hint(&self, window: &Window, cx: &App) -> Option<gpui::Div> {
        let key = Kbd::binding_for_action_in(&NextWalletTab, &self.wallet_focus, window)?;
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    key.outline()
                        .font_family(crate::assets::KEYCAP_FONT_FAMILY)
                        .font_weight(FontWeight::NORMAL)
                        .font_features(gpui::FontFeatures(Arc::new(vec![("case".into(), 1)]))),
                )
                .child(
                    div()
                        .text_size(gpui::rems(11.0 / 16.0))
                        .line_height(gpui::rems(14.0 / 16.0))
                        .text_color(cx.theme().muted_foreground)
                        .child("switch tab"),
                ),
        )
    }

    pub(super) fn render_wallet_content(
        &self,
        root: &Entity<Self>,
        window: &Window,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        match self.active_wallet_tab {
            WalletTab::Private => self.render_private_assets_body(root, cx),
            WalletTab::Public => self.render_public_wallet_body(root, window, cx),
            WalletTab::Activity => self.render_wallet_activity_body(root, cx),
            WalletTab::UTXOs => self
                .render_utxo_body(root, window)
                .debug_selector(|| "wallet-utxos-body".to_owned())
                .into_any_element(),
        }
    }

    pub(in crate::root) fn render_chain_error_body(
        &self,
        root: &Entity<Self>,
        message: &str,
    ) -> gpui::Div {
        let can_retry =
            matches!(self.vault_state, VaultState::ViewUnlocked) && self.view_session.is_some();
        let retry_root = root.clone();

        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .max_w(px(520.0))
                    .text_color(rgb(theme::DANGER))
                    .text_align(gpui::TextAlign::Center)
                    .child(SharedString::from(message.to_owned())),
            )
            .when(can_retry, |this| {
                this.child(
                    app_button("wallet-chain-retry-sync", "Retry sync")
                        .outline()
                        .small()
                        .on_click(move |_event, _window, cx| {
                            retry_root.update(cx, |root, cx| {
                                if root.view_session.is_none() {
                                    return;
                                }
                                let chain_id = root.last_evm_chain;
                                let overrides = chain_load_overrides();
                                root.start_chain_load(chain_id, &overrides, true, cx);
                            });
                        }),
                )
            })
    }
}

fn walletconnect_tab_attention_badge(count: usize) -> impl IntoElement {
    app_status_tag(attention_count_label(count), theme::WARNING)
}

fn attention_count_label(count: usize) -> String {
    if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    }
}
