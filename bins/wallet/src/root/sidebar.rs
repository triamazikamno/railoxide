use gpui::{
    Entity, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, div, img, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Icon, IconName,
    sidebar::{Sidebar, SidebarMenu, SidebarMenuItem},
    tooltip::Tooltip,
};
use ui::clipboard::{clipboard_with_toast, copy_to_clipboard_with_custom_toast};
use ui::theme;

use crate::assets::{LOGO_ICON_PATH, RailgunSidebarIcon, RailgunSocialIcon, SIDEBAR_WORDMARK_PATH};

use super::shell::{
    COPY_URL_TOOLTIP, LINK_COPIED_MESSAGE, RAILOXIDE_REPOSITORY_URL, TELEGRAM_URL,
    wallet_build_label,
};
use super::{
    SIDEBAR_WIDTH, WalletRoot, WalletTab, app_status_tag, rgb_with_alpha, should_focus_utxo_table,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Activity {
    Wallet,
    Broadcaster,
    AddressBook,
    Proposals,
    Settings,
}

impl WalletRoot {
    pub(super) fn render_sidebar(
        &self,
        root: Entity<Self>,
        collapsed: bool,
        sidebar_is_narrow: bool,
    ) -> impl IntoElement {
        let wallet_root = root.clone();
        let broadcaster_root = root.clone();
        let address_book_root = root.clone();
        let proposals_root = root.clone();
        let settings_root = root.clone();
        let public_broadcaster_count = self.sidebar_public_broadcaster_count;
        let public_broadcaster_color =
            Self::public_broadcaster_status_color(public_broadcaster_count);
        let walletconnect_pending_count = self.walletconnect_pending_request_count();
        let walletconnect_attention =
            walletconnect_pending_count > 0 && self.active_activity != Activity::Wallet;

        Sidebar::new("wallet-sidebar")
            .w(SIDEBAR_WIDTH)
            .collapsed(collapsed)
            .header(Self::render_sidebar_header(
                root,
                collapsed,
                sidebar_is_narrow,
            ))
            .child(
                SidebarMenu::new()
                    .child(
                        SidebarMenuItem::new("Wallets")
                            .icon(
                                Icon::new(RailgunSidebarIcon::Wallet)
                                    .size_5()
                                    .when(walletconnect_attention, |icon| {
                                        icon.text_color(rgb(theme::WARNING))
                                    }),
                            )
                            .active(self.active_activity == Activity::Wallet)
                            .when(walletconnect_attention, |item| {
                                item.suffix(move |_window, _cx| {
                                    Self::render_walletconnect_attention_badge(
                                        walletconnect_pending_count,
                                    )
                                })
                            })
                            .on_click(move |_event, _window, cx| {
                                wallet_root.update(cx, |root, cx| {
                                    root.clear_settings_transient_status(cx);
                                    root.invalidate_governance_context();
                                    root.active_activity = Activity::Wallet;
                                    if root.active_wallet_tab == WalletTab::Public {
                                        root.focus_public_account_search_on_render = true;
                                    }
                                    if root.active_wallet_tab == WalletTab::Activity {
                                        root.mark_wallet_activity_seen(cx);
                                    }
                                    root.focus_utxo_table_on_render = should_focus_utxo_table(
                                        root.active_activity,
                                        root.active_wallet_tab,
                                        root.chain_states.get(&root.last_evm_chain),
                                    );
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        SidebarMenuItem::new("Broadcasters")
                            .icon(
                                Icon::new(RailgunSidebarIcon::Broadcaster)
                                    .size_5()
                                    .text_color(rgb(public_broadcaster_color)),
                            )
                            .active(self.active_activity == Activity::Broadcaster)
                            .when(public_broadcaster_count > 0, |item| {
                                item.suffix(move |_window, _cx| {
                                    Self::render_public_broadcaster_count_badge(
                                        public_broadcaster_count,
                                    )
                                })
                            })
                            .on_click(move |_event, window, cx| {
                                broadcaster_root.update(cx, |root, cx| {
                                    root.clear_settings_transient_status(cx);
                                    root.invalidate_governance_context();
                                    root.sync_broadcaster_monitor_chain_filter(
                                        root.last_evm_chain,
                                        window,
                                        cx,
                                    );
                                    root.active_activity = Activity::Broadcaster;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        SidebarMenuItem::new("Address book")
                            .icon(Icon::new(RailgunSidebarIcon::BookUser).size_5())
                            .active(self.active_activity == Activity::AddressBook)
                            .on_click(move |_event, _window, cx| {
                                address_book_root.update(cx, |root, cx| {
                                    root.clear_settings_transient_status(cx);
                                    root.invalidate_governance_context();
                                    root.active_activity = Activity::AddressBook;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        SidebarMenuItem::new("Governance")
                            .icon(Icon::new(RailgunSidebarIcon::Landmark).size_5())
                            .active(self.active_activity == Activity::Proposals)
                            .on_click(move |_event, _window, cx| {
                                proposals_root.update(cx, |root, cx| {
                                    root.clear_settings_transient_status(cx);
                                    root.open_proposals(cx);
                                });
                            }),
                    )
                    .child(
                        SidebarMenuItem::new("Settings")
                            .icon(Icon::new(IconName::Settings).size_5())
                            .active(self.active_activity == Activity::Settings)
                            .on_click(move |_event, _window, cx| {
                                settings_root.update(cx, |root, cx| {
                                    root.clear_settings_transient_status(cx);
                                    root.invalidate_settings_data_folder_size(cx);
                                    root.invalidate_governance_context();
                                    root.refresh_device_auth_status();
                                    root.active_activity = Activity::Settings;
                                    cx.notify();
                                });
                            }),
                    ),
            )
    }

    const fn public_broadcaster_status_color(count: usize) -> u32 {
        if count > 0 {
            theme::SUCCESS
        } else {
            theme::WARNING
        }
    }

    fn render_public_broadcaster_count_badge(count: usize) -> impl IntoElement {
        app_status_tag(count.to_string(), theme::SUCCESS)
    }

    fn render_walletconnect_attention_badge(count: usize) -> impl IntoElement {
        app_status_tag(
            if count > 99 {
                "99+".to_owned()
            } else {
                count.to_string()
            },
            theme::WARNING,
        )
    }

    fn render_sidebar_header(
        root: Entity<Self>,
        collapsed: bool,
        sidebar_is_narrow: bool,
    ) -> impl IntoElement {
        Self::render_sidebar_brand(root, collapsed, sidebar_is_narrow)
    }

    fn render_sidebar_brand(
        root: Entity<Self>,
        collapsed: bool,
        sidebar_is_narrow: bool,
    ) -> impl IntoElement {
        div()
            .w_full()
            .when(!collapsed, |this| {
                this.flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(
                        Self::render_sidebar_brand_toggle(root.clone(), sidebar_is_narrow)
                            .child(Self::render_sidebar_logo())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .flex()
                                    .line_height(gpui::relative(1.2))
                                    .child(Self::render_sidebar_wordmark()),
                            ),
                    )
                    .child(Self::render_sidebar_build_metadata())
            })
            .when(collapsed, |this| {
                this.child(
                    Self::render_sidebar_brand_toggle(root, sidebar_is_narrow)
                        .justify_center()
                        .child(Self::render_sidebar_logo()),
                )
            })
    }

    fn render_sidebar_brand_toggle(
        root: Entity<Self>,
        sidebar_is_narrow: bool,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("sidebar-brand-toggle")
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .on_click(move |_event, _window, cx| {
                root.update(cx, |root, cx| {
                    if sidebar_is_narrow {
                        root.sidebar_narrow_expanded = !root.sidebar_narrow_expanded;
                    } else {
                        root.sidebar_manually_collapsed = !root.sidebar_manually_collapsed;
                    }
                    cx.notify();
                });
            })
    }

    fn render_sidebar_build_metadata() -> gpui::Div {
        let build_label = wallet_build_label();

        div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .child(
                        div()
                            .min_w(px(0.0))
                            .max_w(px(166.0))
                            .truncate()
                            .font_family(theme::APP_MONO_FONT_FAMILY)
                            .text_size(px(10.5))
                            .line_height(px(14.0))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(build_label.clone()),
                    )
                    .child(clipboard_with_toast(
                        "wallet-sidebar-build-info-copy",
                        build_label,
                    )),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .gap_1()
                    .child(Self::render_sidebar_social_copy_button(
                        "wallet-sidebar-repository-url-copy",
                        Icon::new(IconName::Github).size_4(),
                        RAILOXIDE_REPOSITORY_URL,
                    ))
                    .child(Self::render_sidebar_social_copy_button(
                        "wallet-sidebar-telegram-url-copy",
                        Icon::new(RailgunSocialIcon::Telegram).size_4(),
                        TELEGRAM_URL,
                    )),
            )
            .child(
                div()
                    .mt(px(18.0))
                    .w(px(70.0))
                    .h(px(1.0))
                    .rounded_full()
                    .bg(rgb_with_alpha(theme::TEXT_MUTED, 0.13)),
            )
    }

    fn render_sidebar_social_copy_button(
        id: &'static str,
        icon: impl IntoElement,
        url: &'static str,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .size(px(22.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(rgb(theme::TEXT_MUTED))
            .cursor_pointer()
            .hover(|this| {
                this.bg(rgb_with_alpha(theme::SURFACE_HOVER, 0.24))
                    .text_color(rgb(theme::TEXT))
            })
            .tooltip(|window, cx| Tooltip::new(COPY_URL_TOOLTIP).build(window, cx))
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            })
            .on_click(move |_event, window, cx| {
                cx.stop_propagation();
                copy_to_clipboard_with_custom_toast(url, LINK_COPIED_MESSAGE, window, cx);
            })
            .child(icon)
    }

    fn render_sidebar_logo() -> impl IntoElement {
        img(LOGO_ICON_PATH).size(px(32.0)).flex_none()
    }

    fn render_sidebar_wordmark() -> impl IntoElement {
        img(SIDEBAR_WORDMARK_PATH)
            .w(px(154.0))
            .h(px(21.3))
            .flex_none()
    }
}
