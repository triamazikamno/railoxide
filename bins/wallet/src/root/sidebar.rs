use gpui::{
    Anchor, App, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels,
    SharedString, StatefulInteractiveElement, Styled, Window, div, img,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Collapsible, Icon, IconName, Sizable,
    button::{Button, ButtonVariants},
    popover::Popover,
    progress::Progress as UiProgress,
    sidebar::{Sidebar, SidebarItem, SidebarMenu, SidebarMenuItem},
    spinner::Spinner,
    tooltip::Tooltip,
};
use ui::clipboard::{clipboard_with_toast, copy_to_clipboard_with_custom_toast};
use ui::network_status::{
    NetworkStatusTrigger, TorExitIpQueryState, network_status_pill, network_status_popover_content,
    network_status_scroll,
};
use ui::theme;
use wallet_ops::ProverCacheBuildProgress;

use crate::assets::{LOGO_ICON_PATH, RailgunSidebarIcon, RailgunSocialIcon, SIDEBAR_WORDMARK_PATH};

use super::shell::{
    COPY_URL_TOOLTIP, LINK_COPIED_MESSAGE, RAILOXIDE_REPOSITORY_URL, TELEGRAM_URL,
    wallet_build_label,
};
use super::{
    SIDEBAR_WIDTH, WalletRoot, WalletTab, app_status_tag, rgb_with_alpha, should_focus_utxo_table,
};

pub(super) const SIDEBAR_FOOTER_HORIZONTAL_INSET: Pixels = px(12.0);
pub(super) const SIDEBAR_STATUS_PILL_HEIGHT: Pixels = px(42.0);

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
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let wallet_root = root.clone();
        let broadcaster_root = root.clone();
        let address_book_root = root.clone();
        let proposals_root = root.clone();
        let settings_root = root.clone();
        let logs_root = root.clone();
        let network_root = root.clone();
        let gateway_root = root.clone();
        let cache_root = root.clone();
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
                                    root.focus_utxo_table_on_render = should_focus_utxo_table(
                                        root.active_activity,
                                        root.active_wallet_tab,
                                        root.chain_states.get(&root.selected_chain),
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
                                        root.selected_chain,
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
                                    root.invalidate_governance_context();
                                    root.active_activity = Activity::Settings;
                                    cx.notify();
                                });
                            }),
                    ),
            )
            .footer(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .gap_1()
                    .when(!collapsed, Styled::items_start)
                    .when(collapsed, Styled::items_center)
                    .when_some(
                        self.prover_cache_build_progress.clone(),
                        |this, progress| {
                            this.child(self.render_prover_cache_build_pill(
                                &cache_root,
                                collapsed,
                                progress,
                            ))
                        },
                    )
                    .child(self.render_gateway_status_pill(&gateway_root, collapsed))
                    .child(self.render_network_status_pill(&network_root, collapsed))
                    .child(
                        SidebarMenu::new()
                            .w_full()
                            .collapsed(collapsed)
                            .child(
                                SidebarMenuItem::new("Logs")
                                    .icon(Icon::new(RailgunSidebarIcon::Logs).size_4())
                                    .active(self.logs_open)
                                    .on_click(move |_event, _window, cx| {
                                        logs_root.update(cx, |root, cx| {
                                            root.logs_open = !root.logs_open;
                                            cx.notify();
                                        });
                                    }),
                            )
                            .render("wallet-sidebar-footer-menu", window, cx),
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

    fn render_network_status_pill(&self, root: &Entity<Self>, collapsed: bool) -> impl IntoElement {
        let status = self.network_status_presentation();
        let expanded_tor = !collapsed && status.kind().is_tor();
        let activity = self.network_activity_presentation();
        let download_rate = self.tor_download_rate;
        let popover_root = root.clone();
        let content_root = root.clone();
        let network_status_error = self.network_status_error.clone();
        let tor_exit_ip_query = self.tor_exit_ip_query.clone();
        let tor_state_reset_confirming = self.tor_state_reset_confirming;

        let trigger = network_status_pill(
            "wallet-network-status-pill-trigger",
            collapsed,
            &status,
            activity.as_ref(),
            download_rate,
            SIDEBAR_WIDTH - SIDEBAR_FOOTER_HORIZONTAL_INSET - SIDEBAR_FOOTER_HORIZONTAL_INSET,
        );

        let trigger = if expanded_tor {
            trigger
                .w(SIDEBAR_WIDTH
                    - SIDEBAR_FOOTER_HORIZONTAL_INSET
                    - SIDEBAR_FOOTER_HORIZONTAL_INSET)
                .min_w(px(0.0))
                .flex_shrink(1.0)
        } else {
            trigger
        };

        let popover = Popover::new("wallet-network-status-popover")
            .p_0()
            .anchor(Anchor::BottomLeft)
            .open(self.network_status_popover_open)
            .on_open_change(move |open, window, cx| {
                popover_root.update(cx, |root, cx| {
                    root.set_network_status_popover_open(*open, cx);
                    if !open {
                        root.network_status_focus.focus(window, cx);
                    }
                });
            })
            .trigger(NetworkStatusTrigger::new(
                trigger,
                &self.network_status_focus,
                &status,
            ))
            .content(move |_state, window, _cx| {
                let root = content_root.clone();
                let copy = match tor_exit_ip_query {
                    TorExitIpQueryState::Success(ip) => Some(
                        clipboard_with_toast("wallet-network-exit-ip-copy", ip.to_string())
                            .into_any_element(),
                    ),
                    _ => None,
                };
                network_status_scroll(
                    network_status_popover_content(
                        &status,
                        network_status_error.clone(),
                        tor_exit_ip_query.clone(),
                        tor_state_reset_confirming,
                        activity.as_ref(),
                        download_rate,
                        move |intent, _window, cx| {
                            root.update(cx, |root, cx| root.network_status_intent(intent, cx));
                        },
                        copy,
                    ),
                    window,
                )
            });

        if expanded_tor {
            div()
                .w(SIDEBAR_WIDTH
                    - SIDEBAR_FOOTER_HORIZONTAL_INSET
                    - SIDEBAR_FOOTER_HORIZONTAL_INSET)
                .min_w(px(0.0))
                .child(popover)
                .into_any_element()
        } else {
            popover.into_any_element()
        }
    }

    fn render_prover_cache_build_pill(
        &self,
        root: &Entity<Self>,
        collapsed: bool,
        progress: ProverCacheBuildProgress,
    ) -> impl IntoElement {
        let popover_root = root.clone();
        let content_progress = progress;
        let trigger = Button::new("wallet-prover-cache-build-pill-trigger")
            .accessibility_label("Prover cache build status")
            .text()
            .tab_stop(false)
            .tooltip("Building prover cache")
            .child(Self::render_prover_cache_build_chip(collapsed));

        Popover::new("wallet-prover-cache-build-popover")
            .open(self.prover_cache_build_popover_open)
            .on_open_change(move |open, _window, cx| {
                popover_root.update(cx, |root, cx| {
                    root.set_prover_cache_build_popover_open(*open, cx);
                });
            })
            .trigger(trigger)
            .content(move |_state, _window, _cx| {
                Self::render_prover_cache_build_popover_content(&content_progress)
            })
    }

    fn render_prover_cache_build_chip(collapsed: bool) -> gpui::AnyElement {
        let color = theme::INFO;
        let spinner = Spinner::new()
            .icon(IconName::LoaderCircle)
            .color(rgb(color).into())
            .with_size(px(14.0));

        if collapsed {
            return div()
                .id("wallet-prover-cache-build-pill-collapsed")
                .h(px(32.0))
                .px_2()
                .flex()
                .items_center()
                .justify_center()
                .rounded_lg()
                .border_1()
                .border_color(rgb(color))
                .bg(rgb_with_alpha(color, 0.08))
                .cursor_pointer()
                .hover(|this| this.bg(rgb_with_alpha(color, 0.14)))
                .child(spinner)
                .into_any_element();
        }

        div()
            .id("wallet-prover-cache-build-pill")
            .h_7()
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(rgb(color))
            .bg(rgb_with_alpha(color, 0.08))
            .text_color(rgb(color))
            .cursor_pointer()
            .hover(|this| this.bg(rgb_with_alpha(color, 0.14)))
            .child(spinner)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .line_height(gpui::relative(1.0))
                    .text_color(rgb(color))
                    .child("Building prover cache"),
            )
            .into_any_element()
    }

    fn render_prover_cache_build_popover_content(progress: &ProverCacheBuildProgress) -> gpui::Div {
        let percent = progress.percent();
        let variant = progress
            .current_variant
            .as_deref()
            .unwrap_or("Preparing variants");
        let variant_kind = match progress.current_variant_is_poi {
            Some(true) => "POI",
            Some(false) => "Railgun",
            None => "Variant",
        };
        let count_text = if progress.total_variants == 0 {
            "Preparing variant list...".to_string()
        } else {
            format!(
                "{} of {} variants complete",
                progress.completed_variants, progress.total_variants
            )
        };

        div()
            .w(px(320.0))
            .flex()
            .flex_col()
            .gap_3()
            .text_size(px(13.0))
            .text_color(rgb(theme::TEXT))
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Spinner::new()
                            .icon(IconName::LoaderCircle)
                            .color(rgb(theme::INFO).into())
                            .with_size(px(16.0)),
                    )
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(theme::INFO))
                            .child(progress.stage.label()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        UiProgress::new("prover-cache-build-progress")
                            .flex_1()
                            .h(px(7.0))
                            .value(f32::from(percent)),
                    )
                    .child(
                        div()
                            .w(px(42.0))
                            .text_color(rgb(theme::INFO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(SharedString::from(format!("{percent}%"))),
                    ),
            )
            .child(
                div()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .line_height(px(18.0))
                    .child(SharedString::from(count_text)),
            )
            .child(
                div()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(theme::SURFACE))
                    .p(px(10.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(variant_kind),
                    )
                    .child(
                        div()
                            .font_family(theme::APP_MONO_FONT_FAMILY)
                            .text_size(px(12.0))
                            .line_height(px(17.0))
                            .text_color(rgb(theme::TEXT))
                            .child(SharedString::from(variant.to_string())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_size(px(12.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(SharedString::from(format!(
                        "Succeeded: {}",
                        progress.succeeded_variants
                    )))
                    .child(SharedString::from(format!(
                        "Failed: {}",
                        progress.failed_variants
                    ))),
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
