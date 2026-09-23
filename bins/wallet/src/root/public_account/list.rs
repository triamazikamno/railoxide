use super::{commands::AccountCommand, public_account_matches_search};
use crate::root::{
    WalletRoot,
    actions::{
        ActivateFocusedAsset, CopySelectedAddress, FocusNextAsset, FocusPreviousAsset,
        FocusPublicAccountSearch, PUBLIC_ACCOUNT_LIST_KEY_CONTEXT,
        PUBLIC_ACCOUNT_SEARCH_KEY_CONTEXT, PUBLIC_ACCOUNT_SECTION_KEY_CONTEXT,
        RenameSelectedAccount, SelectNextAccount, SelectPreviousAccount,
    },
    public_balances::public_active_usd_total,
};
use gpui::{
    App, Context, Entity, Focusable, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, canvas, div, prelude::FluentBuilder, rems,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable,
    button::ButtonVariants,
    checkbox::Checkbox,
    kbd::Kbd,
    menu::{ContextMenuExt, DropdownMenu, PopupMenuItem},
    scroll::ScrollableElement,
};
use gpui_kit::base::{Accordion, AccordionHeader, AccordionItem, AccordionPanel, AccordionTrigger};
use railgun_ui::{format_usd_micro_value, short_address};
use std::sync::Arc;
use ui::controls::{
    app_button, app_button_base, app_button_label, app_input, app_muted_text, app_strong_text,
};
use wallet_ops::vault::{PublicAccountSource, PublicAccountStatus};

use std::cmp::Reverse;

use wallet_ops::{
    PublicBalanceSnapshot, TokenAnchorRateCache, settings::PublicAccountSort,
    vault::PublicAccountMetadata,
};

use super::public_account_display_label;
use crate::root::public_balances::{
    public_account_visible_balances_for_chain, public_balance_compact_usd,
    public_balances_usd_total,
};

// Design dimensions in units at the default 16px rem. Rendering and fit arithmetic
// use the same scale; shell insets remain the existing physical window geometry.
pub(super) const CONTENT_WIDTH: f32 = 980.0;
pub(super) const IDENTICON_WIDTH: f32 = 28.0;
pub(super) const IDENTITY_WIDTH: f32 = 200.0;
pub(super) const TOTAL_WIDTH: f32 = 110.0;
pub(super) const COLUMN_GAP: f32 = 12.0;
pub(super) const ROW_INSET: f32 = 10.0;
pub(super) const TILE_WIDTH: f32 = 162.0;
pub(super) const TILE_HEIGHT: f32 = 40.0;
pub(super) const TILE_GAP: f32 = 6.0;
pub(super) const MORE_WIDTH: f32 = 56.0;

#[allow(clippy::cast_sign_loss)] // Widths are clamped to nonnegative before conversion.
pub(in crate::root) fn tiles_that_fit(width: f32, asset_count: usize) -> usize {
    let fit = ((width + TILE_GAP).max(0.0) / (TILE_WIDTH + TILE_GAP)).floor() as usize;
    if fit >= asset_count {
        asset_count
    } else {
        // Keep at least one asset visible. The row's scroll region preserves
        // access at widths below one tile plus the overflow control.
        (((width - MORE_WIDTH).max(0.0) / (TILE_WIDTH + TILE_GAP)).floor() as usize).max(1)
    }
}

pub(in crate::root) fn balance_column_width(
    viewport: f32,
    sidebar_collapsed: bool,
    pricing: bool,
    scale: f32,
) -> f32 {
    let sidebar = if sidebar_collapsed { 48.0 } else { 220.0 };
    let content = ((viewport - sidebar - 24.0) / scale).min(CONTENT_WIDTH);
    let columns = COLUMN_GAP.mul_add(2.0, IDENTICON_WIDTH + IDENTITY_WIDTH)
        + if pricing {
            TOTAL_WIDTH + COLUMN_GAP
        } else {
            0.0
        };
    (ROW_INSET.mul_add(-2.0, content) - columns).max(0.0)
}

pub(in crate::root) fn sort_public_accounts(
    accounts: &mut [PublicAccountMetadata],
    sort: PublicAccountSort,
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    pricing: bool,
    cache: Option<&TokenAnchorRateCache>,
) {
    let sort = effective_public_account_sort(sort, pricing);
    match sort {
        PublicAccountSort::Added => accounts.sort_by_key(|account| account.display_order),
        PublicAccountSort::Name => accounts.sort_by_cached_key(account_name_key),
        PublicAccountSort::Value => accounts.sort_by_cached_key(|account| {
            let balances = public_account_visible_balances_for_chain(
                snapshot,
                chain_id,
                &account.public_account_uuid,
                account.status,
            );
            let total = public_balances_usd_total(&balances, chain_id, cache);
            (
                balances.is_empty(),
                Reverse(total.value.unwrap_or_default()),
                account_name_key(account),
            )
        }),
    }
}

fn account_name_key(account: &PublicAccountMetadata) -> (bool, String, u32) {
    let label = public_account_display_label(account);
    (
        label.is_none(),
        label.unwrap_or_default().to_lowercase(),
        account.display_order,
    )
}

pub(in crate::root) const fn effective_public_account_sort(
    sort: PublicAccountSort,
    pricing: bool,
) -> PublicAccountSort {
    match sort {
        PublicAccountSort::Value if !pricing => PublicAccountSort::Name,
        _ => sort,
    }
}

pub(super) fn dimension(value: f32) -> gpui::Rems {
    rems(value / 16.0)
}

impl WalletRoot {
    pub(super) fn public_chain_has_pricing(&self) -> bool {
        self.effective_chain_configs
            .get(self.selected_chain)
            .is_some_and(|chain| chain.native_usd_oracle.is_some())
    }

    pub(in crate::root) fn public_list_accounts(
        &self,
    ) -> (Vec<PublicAccountMetadata>, Vec<PublicAccountMetadata>) {
        let (mut active, mut inactive): (Vec<_>, Vec<_>) = self
            .public_accounts
            .iter()
            .filter(|account| account.is_available_on_chain(self.selected_chain))
            .filter(|account| {
                public_account_matches_search(account, &self.public_form.search_query)
            })
            .filter(|account| {
                account.status != PublicAccountStatus::Active
                    || !self.ui_state.public_hide_empty_accounts
                    || !self
                        .public_account_visible_balances(
                            &account.public_account_uuid,
                            account.status,
                        )
                        .is_empty()
            })
            .cloned()
            .partition(|account| account.status == PublicAccountStatus::Active);
        for accounts in [&mut active, &mut inactive] {
            sort_public_accounts(
                accounts,
                self.ui_state.public_account_sort,
                self.public_balance_snapshot.as_deref(),
                self.selected_chain,
                self.public_chain_has_pricing(),
                Some(&self.public_broadcaster_anchor_cache),
            );
        }
        (active, inactive)
    }

    const fn effective_public_account_section(
        &self,
        active: &[PublicAccountMetadata],
        inactive: &[PublicAccountMetadata],
    ) -> PublicAccountStatus {
        match self.public_form.open_section {
            PublicAccountStatus::Active if active.is_empty() && !inactive.is_empty() => {
                PublicAccountStatus::Inactive
            }
            PublicAccountStatus::Inactive if inactive.is_empty() && !active.is_empty() => {
                PublicAccountStatus::Active
            }
            section => section,
        }
    }

    fn public_open_section_accounts(&self) -> Vec<PublicAccountMetadata> {
        let (active, inactive) = self.public_list_accounts();
        match self.effective_public_account_section(&active, &inactive) {
            PublicAccountStatus::Active => active,
            PublicAccountStatus::Inactive => inactive,
        }
    }

    pub(in crate::root) fn reconcile_public_account_selection(&mut self) {
        let accounts = self.public_open_section_accounts();
        if accounts.iter().any(|account| {
            Some(account.public_account_uuid.as_str())
                == self.public_form.selected_account_uuid.as_deref()
        }) {
            return;
        }
        if let Some(first) = accounts.first() {
            self.reset_public_asset_focus();
            self.public_form.selected_asset = None;
            self.public_form.selected_account_uuid =
                Some(Arc::from(first.public_account_uuid.as_str()));
            self.public_form.selected_row_bounds.set(None);
            self.public_form
                .list_scroll
                .set_offset(gpui::Point::default());
            self.remember_public_account_selection();
            self.publish_gateway_desktop_state();
        }
    }

    pub(super) fn public_account_neighbour(&self, uuid: &str) -> Option<Arc<str>> {
        let accounts = self.public_open_section_accounts();
        let index = accounts
            .iter()
            .position(|account| account.public_account_uuid == uuid)?;
        accounts
            .get(index + 1)
            .or_else(|| index.checked_sub(1).and_then(|index| accounts.get(index)))
            .map(|account| Arc::from(account.public_account_uuid.as_str()))
    }

    pub(in crate::root) fn reset_public_asset_focus(&mut self) {
        self.public_form.focused_asset_index = None;
        self.public_form.asset_menu = None;
        self.public_form.asset_menu_subscription = None;
    }

    pub(super) fn select_public_account_row(
        &mut self,
        uuid: Option<Arc<str>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.selected_account_uuid != uuid {
            self.reset_public_asset_focus();
            self.public_form.selected_asset = None;
            self.public_form.selected_account_uuid = uuid;
            self.remember_public_account_selection();
            self.publish_gateway_desktop_state();
        }
        self.public_form.list_focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn move_public_account_selection(
        &mut self,
        next: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.public_form.list_focus.focus(window, cx);
        let accounts = self.public_open_section_accounts();
        if accounts.is_empty() {
            return;
        }
        let current = accounts.iter().position(|account| {
            Some(account.public_account_uuid.as_str())
                == self.public_form.selected_account_uuid.as_deref()
        });
        let index = current.map_or(if next { 0 } else { accounts.len() - 1 }, |index| {
            if next {
                (index + 1).min(accounts.len() - 1)
            } else {
                index.saturating_sub(1)
            }
        });
        self.select_public_account_row(
            Some(Arc::from(accounts[index].public_account_uuid.as_str())),
            window,
            cx,
        );
        self.scroll_selected_public_row_into_view(window);
    }

    /// Scrolls the list just enough to show the selected row. The row's bounds
    /// are captured while it paints, so this waits for the frame that renders
    /// the new selection before measuring.
    pub(super) fn scroll_selected_public_row_into_view(&self, window: &Window) {
        let handle = self.public_form.list_scroll.clone();
        let row_bounds = self.public_form.selected_row_bounds.clone();
        let focus = self.public_form.list_focus.clone();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, _| {
                if !focus.is_focused(window) {
                    return;
                }
                let Some(row) = row_bounds.get() else {
                    return;
                };
                let viewport = handle.bounds();
                let margin = gpui::px(8.0);
                let mut offset = handle.offset();
                if row.top() - margin < viewport.top() {
                    offset.y += viewport.top() - (row.top() - margin);
                } else if row.bottom() + margin > viewport.bottom() {
                    offset.y -= (row.bottom() + margin) - viewport.bottom();
                } else {
                    return;
                }
                offset.y = offset.y.clamp(-handle.max_offset().y, gpui::px(0.0));
                handle.set_offset(offset);
                window.refresh();
            });
        });
    }

    pub(super) fn render_public_accounts_summary(
        &self,
        root: &Entity<Self>,
        cx: &App,
    ) -> impl IntoElement {
        let total = public_active_usd_total(
            self.public_balance_snapshot.as_deref(),
            &self.public_accounts,
            self.selected_chain,
            Some(&self.public_broadcaster_anchor_cache),
        );
        let subtle = cx.theme().muted_foreground;
        let caption = |text: String| {
            div()
                .text_size(dimension(11.0))
                .line_height(dimension(14.0))
                .text_color(subtle)
                .child(text)
        };
        let mut value = div().flex().items_baseline().gap_2().min_w_0();
        if self.public_chain_has_pricing() {
            let label = total.value.map_or_else(
                || {
                    if self.public_balance_snapshot.is_some() && !total.partial {
                        "$0.00".into()
                    } else {
                        "Unavailable".into()
                    }
                },
                format_usd_micro_value,
            );
            value = value
                .child(
                    div()
                        .text_size(dimension(18.0))
                        .line_height(dimension(24.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(label),
                )
                .when(total.partial, |value| {
                    value.child(app_muted_text("partial").text_xs())
                });
        } else {
            let root = root.clone();
            let chain_id = self.selected_chain;
            value = value
                .items_center()
                .child(app_muted_text("No USD pricing on this chain"))
                .child(
                    app_button("public-set-price-source", "Set a price source…")
                        .link()
                        .xsmall()
                        .compact()
                        .on_click(move |_, window, cx| {
                            root.update(cx, |root, cx| {
                                root.open_chain_editor(chain_id, window, cx);
                            });
                        }),
                );
        }
        let refreshed = self
            .public_balance_snapshot
            .as_ref()
            .filter(|snapshot| snapshot.chain_id == self.selected_chain)
            .map_or_else(
                || "Not refreshed".to_owned(),
                |snapshot| relative_time_label(snapshot.refreshed_at),
            );
        let status = if self.public_balance_refreshing {
            app_muted_text("Refreshing…")
        } else if let Some(error) = &self.public_balance_error {
            app_muted_text(format!("Stale · {error}")).text_color(cx.theme().warning)
        } else {
            app_muted_text(refreshed)
        };
        div()
            .flex()
            .items_center()
            .gap_6()
            .flex_none()
            .bg(cx.theme().background)
            .border_color(cx.theme().border)
            .px(dimension(14.0))
            .py(dimension(10.0))
            .border_1()
            .rounded_md()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(caption("Total balance".into()))
                    .child(value),
            )
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(caption("Last refreshed".into()))
                    .child(status.text_sm().truncate()),
            )
            .child(div().flex_1())
            .child(self.render_stealth_accounts_button(root, cx))
    }

    pub(super) fn public_selected_chain_name(&self) -> String {
        self.effective_chain_configs
            .get(self.selected_chain)
            .map_or_else(
                || {
                    railgun_ui::chain_name(self.selected_chain)
                        .unwrap_or("selected chain")
                        .to_owned()
                },
                |chain| chain.name.clone(),
            )
    }

    pub(in crate::root) fn render_public_list_controls(
        &self,
        root: &Entity<Self>,
    ) -> impl IntoElement {
        let pricing = self.public_chain_has_pricing();
        let sort = effective_public_account_sort(self.ui_state.public_account_sort, pricing);
        let sort_root = root.clone();
        let hide_root = root.clone();
        let search = self.public_form.search_input.clone();
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .debug_selector(|| "public-account-search".into())
                    .w(dimension(260.0))
                    .max_w_full()
                    .key_context(PUBLIC_ACCOUNT_SEARCH_KEY_CONTEXT)
                    .on_action({
                        let root = root.clone();
                        move |_: &SelectPreviousAccount, window, cx| {
                            root.update(cx, |root, cx| {
                                root.move_public_account_selection(false, window, cx);
                            });
                        }
                    })
                    .on_action({
                        let root = root.clone();
                        move |_: &SelectNextAccount, window, cx| {
                            root.update(cx, |root, cx| {
                                root.move_public_account_selection(true, window, cx);
                            });
                        }
                    })
                    .child(app_input(&search).small().when(
                        !self.public_form.search_query.is_empty(),
                        |input| {
                            input.suffix(
                                app_button_base("public-clear-search")
                                    .debug_selector(|| "public-clear-search".into())
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Close)
                                    .accessibility_label("Clear search")
                                    .tooltip("Clear search")
                                    .on_click(move |_, window, cx| {
                                        search.update(cx, |input, cx| {
                                            input.replace_all("", window, cx);
                                        });
                                    }),
                            )
                        },
                    )),
            )
            .child(
                app_button_base("public-account-sort")
                    .debug_selector(|| "public-account-sort".into())
                    .outline()
                    .small()
                    .dropdown_caret(true)
                    .accessibility_label(format!("Sort by {}", sort_label(sort)))
                    .icon(
                        gpui_component::Icon::new(
                            crate::assets::RailgunActionIcon::ListSortDescending,
                        )
                        .text_color(gpui::rgb(ui::theme::TEXT_MUTED)),
                    )
                    .child(app_button_label(sort_label(sort)))
                    .dropdown_menu(move |mut menu, _, _| {
                        for choice in [
                            PublicAccountSort::Value,
                            PublicAccountSort::Name,
                            PublicAccountSort::Added,
                        ] {
                            let root = sort_root.clone();
                            menu = menu.item(
                                PopupMenuItem::new(sort_label(choice))
                                    .checked(sort == choice)
                                    .disabled(choice == PublicAccountSort::Value && !pricing)
                                    .on_click(move |_, _, cx| {
                                        root.update(cx, |root, cx| {
                                            root.ui_state.public_account_sort = choice;
                                            root.reconcile_public_account_selection();
                                            root.save_ui_state();
                                            cx.notify();
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .child(
                Checkbox::new("public-hide-empty")
                    .debug_selector(|| "public-hide-empty".into())
                    .small()
                    .label("Hide empty")
                    .checked(self.ui_state.public_hide_empty_accounts)
                    .on_click(move |checked, _, cx| {
                        hide_root.update(cx, |root, cx| {
                            root.ui_state.public_hide_empty_accounts = *checked;
                            root.reconcile_public_account_selection();
                            root.save_ui_state();
                            cx.notify();
                        });
                    }),
            )
    }

    pub(in crate::root) fn render_public_account_list(
        &self,
        root: &Entity<Self>,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        let pricing = self.public_chain_has_pricing();
        let (active, inactive) = self.public_list_accounts();
        let search_active = !self.public_form.search_query.is_empty();
        let sidebar_collapsed =
            if window.viewport_size().width < crate::root::SIDEBAR_AUTO_COLLAPSE_WIDTH {
                !self.sidebar_narrow_expanded
            } else {
                self.sidebar_manually_collapsed
            };
        let width = balance_column_width(
            window.viewport_size().width.into(),
            sidebar_collapsed,
            pricing,
            f32::from(window.rem_size()) / 16.0,
        );
        let focus_color = cx.theme().primary;
        let mut list = div()
            .id("public-account-list")
            .key_context(PUBLIC_ACCOUNT_LIST_KEY_CONTEXT)
            .track_focus(&self.public_form.list_focus)
            .tab_index(0)
            .tab_stop(true)
            .on_action({
                let root = root.clone();
                move |_: &FocusPublicAccountSearch, window, cx| {
                    root.update(cx, |root, cx| {
                        root.reset_public_asset_focus();
                        root.public_form
                            .search_input
                            .focus_handle(cx)
                            .focus(window, cx);
                        cx.notify();
                    });
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &SelectPreviousAccount, window, cx| {
                    root.update(cx, |root, cx| {
                        root.move_public_account_selection(false, window, cx);
                    });
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &SelectNextAccount, window, cx| {
                    root.update(cx, |root, cx| {
                        root.move_public_account_selection(true, window, cx);
                    });
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &FocusPreviousAsset, window, cx| {
                    root.update(cx, |root, cx| {
                        root.move_public_asset_focus(false, window, cx);
                    });
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &FocusNextAsset, window, cx| {
                    root.update(cx, |root, cx| {
                        root.move_public_asset_focus(true, window, cx);
                    });
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &ActivateFocusedAsset, window, cx| {
                    root.update(cx, |root, cx| {
                        root.activate_public_focused_asset(window, cx);
                    });
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &CopySelectedAddress, window, cx| {
                    root.update(cx, |root, cx| {
                        if let Some(uuid) = root.public_form.selected_account_uuid.clone() {
                            root.run_public_account_command(
                                &uuid,
                                AccountCommand::Copy,
                                window,
                                cx,
                            );
                        }
                    });
                }
            })
            .on_action({
                let root = root.clone();
                move |_: &RenameSelectedAccount, window, cx| {
                    root.update(cx, |root, cx| {
                        if let Some(uuid) = root.public_form.selected_account_uuid.clone() {
                            root.run_public_account_command(
                                &uuid,
                                AccountCommand::Rename,
                                window,
                                cx,
                            );
                        }
                    });
                }
            })
            .w_full()
            .flex_1()
            .min_h(gpui::px(0.0))
            .flex()
            .flex_col()
            .gap_3()
            .focus_visible(move |style| style.border_color(focus_color));
        if search_active {
            list = list.child(
                app_muted_text(format!(
                    "{} active · {} inactive match",
                    active.len(),
                    inactive.len()
                ))
                .text_xs(),
            );
        }
        let open_section = self.effective_public_account_section(&active, &inactive);
        list.child(
            Accordion::new("public-account-sections")
                .flex_1()
                .min_h(gpui::px(0.0))
                .flex()
                .flex_col()
                .child(self.render_public_account_section(
                    root,
                    PublicAccountStatus::Active,
                    &active,
                    open_section == PublicAccountStatus::Active,
                    width,
                    window,
                    cx,
                ))
                .child(self.render_public_account_section(
                    root,
                    PublicAccountStatus::Inactive,
                    &inactive,
                    open_section == PublicAccountStatus::Inactive,
                    width,
                    window,
                    cx,
                )),
        )
    }

    fn render_public_account_section(
        &self,
        root: &Entity<Self>,
        status: PublicAccountStatus,
        accounts: &[PublicAccountMetadata],
        open: bool,
        width: f32,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        let inactive = status == PublicAccountStatus::Inactive;
        let id = if inactive {
            "public-inactive"
        } else {
            "public-active"
        };
        let disabled = accounts.is_empty();
        let toggle_root = root.clone();
        let focus_color = cx.theme().primary;
        let hover = cx.theme().list_hover;
        let mut trigger = AccordionTrigger::new(id)
            .debug_selector(move || id.into())
            .open(open)
            .disabled(disabled)
            .flex_1()
            .min_w(gpui::px(0.0))
            .h(dimension(30.0))
            .px(dimension(ROW_INSET))
            .flex()
            .items_center()
            .gap_2()
            .text_color(if open {
                cx.theme().foreground
            } else {
                cx.theme().muted_foreground
            })
            .when(disabled, |trigger| trigger.opacity(0.5))
            .when(!disabled, |trigger| {
                let keyboard_root = root.clone();
                trigger
                    .tab_index(0)
                    .key_context(PUBLIC_ACCOUNT_SECTION_KEY_CONTEXT)
                    .on_action(move |_: &gpui_kit::base::actions::Confirm, window, cx| {
                        if !open {
                            keyboard_root.update(cx, |root, cx| {
                                root.set_public_account_section_open(status, window, cx);
                            });
                        }
                    })
                    .hover(move |style| style.bg(hover))
                    .focus_visible(move |style| style.border_1().border_color(focus_color))
            })
            .child(
                app_button_label(format!(
                    "{} · {}",
                    if inactive { "INACTIVE" } else { "ACTIVE" },
                    accounts.len()
                ))
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD),
            )
            .child(div().flex_1())
            // Key hints live in the Active header whichever section is open;
            // on narrow windows they clip from the right instead of wrapping.
            .when(!inactive, |trigger| {
                trigger.child(
                    div()
                        .min_w(gpui::px(0.0))
                        .overflow_hidden()
                        .child(self.render_public_account_hints(window, cx)),
                )
            })
            .on_change(move |requested_open, _, window, cx| {
                if requested_open {
                    toggle_root.update(cx, |root, cx| {
                        root.set_public_account_section_open(status, window, cx);
                    });
                }
            });
        if inactive && open && !disabled {
            let fetch_root = root.clone();
            trigger = trigger.child(
                app_button("public-fetch-inactive", "Fetch balances")
                    .outline()
                    .xsmall()
                    .loading(self.public_inactive_balance_refreshing)
                    .disabled(self.public_inactive_balance_refreshing)
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        fetch_root.update(cx, |root, cx| {
                            root.schedule_inactive_public_balance_refresh(cx);
                        });
                    }),
            );
        }
        trigger = trigger.child(
            Icon::new(if open {
                IconName::ChevronUp
            } else {
                IconName::ChevronDown
            })
            .xsmall()
            .text_color(cx.theme().muted_foreground),
        );
        let header = AccordionHeader::new(trigger)
            .id(format!("{id}-header"))
            .flex_none()
            .flex()
            .items_center()
            .when(open, |header| {
                header.border_b_1().border_color(cx.theme().border)
            })
            .when(inactive && !open, |header| {
                header.border_t_1().border_color(cx.theme().border)
            });
        let mut rows = div().w_full().flex().flex_col();
        if inactive {
            rows = rows.children(self.public_inactive_balance_error.as_ref().map(|error| {
                app_muted_text(error.to_string())
                    .text_color(cx.theme().warning)
                    .whitespace_normal()
            }));
        }
        if accounts.is_empty() {
            rows = rows.child(
                app_muted_text(if self.public_accounts.is_empty() {
                    "No Public accounts yet. Add a derived account or import a private key."
                } else {
                    "No Public accounts match these filters."
                })
                .p_2(),
            );
        } else if open {
            for (index, account) in accounts.iter().enumerate() {
                rows = rows.child(self.render_public_account_row(
                    root,
                    account,
                    index == 0,
                    width,
                    window,
                    cx,
                ));
            }
        }
        AccordionItem::new()
            .open(open)
            .disabled(disabled)
            .flex()
            .flex_col()
            .when(open, |item| item.flex_1().min_h(gpui::px(0.0)))
            .when(!open, Styled::flex_none)
            .header(header)
            .panel(
                AccordionPanel::new()
                    .id(format!("{id}-panel"))
                    .open(open)
                    .keep_mounted(false)
                    .flex_1()
                    .min_h(gpui::px(0.0))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("public-account-scroll")
                            .flex_1()
                            .min_h(gpui::px(0.0))
                            .overflow_y_scroll()
                            .track_scroll(&self.public_form.list_scroll)
                            .vertical_scrollbar(&self.public_form.list_scroll)
                            .child(rows),
                    ),
            )
    }
}

const fn sort_label(sort: PublicAccountSort) -> &'static str {
    match sort {
        PublicAccountSort::Value => "Value",
        PublicAccountSort::Name => "Name",
        PublicAccountSort::Added => "Added",
    }
}

impl WalletRoot {
    #[allow(clippy::too_many_lines)] // One row renderer keeps the column contract in one place.
    fn render_public_account_row(
        &self,
        root: &Entity<Self>,
        account: &PublicAccountMetadata,
        first: bool,
        width: f32,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        let selected = self.public_form.selected_account_uuid.as_deref()
            == Some(account.public_account_uuid.as_str());
        let uuid: Arc<str> = Arc::from(account.public_account_uuid.as_str());
        let group = SharedString::from(format!("public-row-{uuid}"));
        let pricing = self.public_chain_has_pricing();
        let balances = self.public_account_sorted_balances(account);
        let total = public_balances_usd_total(
            &balances,
            self.selected_chain,
            Some(&self.public_broadcaster_anchor_cache),
        );
        let dim = account.status == PublicAccountStatus::Inactive || balances.is_empty();
        let label = public_account_display_label(account);
        let address = short_address(&account.address);
        let subtle = cx.theme().muted_foreground;
        let name = label.clone().map_or_else(
            || {
                app_muted_text(address.clone())
                    .font_family(ui::theme::APP_MONO_FONT_FAMILY)
                    .text_sm()
            },
            |label| {
                app_strong_text(label)
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
            },
        );
        let mut identity_line = div()
            .flex()
            .items_center()
            .gap(dimension(6.0))
            .min_w_0()
            .child(name.truncate().when(dim, |text| text.text_color(subtle)));
        let shortcuts = |root: &Entity<Self>| {
            let mut shortcuts = div()
                .flex()
                .items_center()
                .flex_none()
                .ml_1()
                .opacity(if selected { 1.0 } else { 0.0 })
                .group_hover(group.clone(), |style| style.opacity(1.0));
            for (command, icon) in [
                (
                    AccountCommand::Copy,
                    gpui_component::Icon::new(IconName::Copy),
                ),
                (
                    AccountCommand::Qr,
                    gpui_component::Icon::new(crate::assets::RailgunActionIcon::QrCode),
                ),
            ] {
                let root = root.clone();
                let uuid = uuid.clone();
                shortcuts = shortcuts.child(
                    super::components::public_account_icon_button(
                        SharedString::from(format!("public-shortcut-{}-{}", command.label(), uuid)),
                        icon,
                        command.label(),
                    )
                    .tab_stop(false)
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        root.update(cx, |root, cx| {
                            root.run_public_account_command(&uuid, command, window, cx);
                        });
                    }),
                );
            }
            shortcuts
        };
        let tag = |text: &'static str, color: gpui::Hsla| {
            div()
                .flex_none()
                .px(dimension(6.0))
                .rounded_full()
                .border_1()
                .border_color(color)
                .bg(color.alpha(0.12))
                .text_color(color)
                .text_size(dimension(10.0))
                .line_height(dimension(14.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(text)
        };
        let mut tags = div().flex().items_center().gap_1().flex_none();
        if account.source != PublicAccountSource::Derived {
            tags = tags.child(tag(
                super::public_account_source_label(account.source),
                gpui::rgb(ui::theme::TEXT_MUTED).into(),
            ));
        }
        if account.is_global() {
            tags = tags.child(tag("Global", cx.theme().info));
        }
        let sessions = self.walletconnect_account_session_count(&uuid);
        if sessions > 0 {
            tags = tags.child(
                div()
                    .id(SharedString::from(format!("public-session-marker-{uuid}")))
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(format!(
                            "WalletConnect sessions · {sessions}"
                        ))
                        .build(window, cx)
                    })
                    .child(
                        crate::root::walletconnect::walletconnect_logo_with_presence(
                            window.rem_size(),
                            true,
                        ),
                    ),
            );
        }
        identity_line = identity_line.child(tags);
        if label.is_none() {
            identity_line = identity_line.child(shortcuts(root));
        }
        let mut identity = div()
            .id(SharedString::from(format!("public-identity-{uuid}")))
            .w(dimension(IDENTITY_WIDTH))
            .flex_none()
            .flex()
            .flex_col()
            .min_w_0()
            .child(identity_line);
        if label.is_some() {
            identity = identity.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        app_muted_text(address)
                            .font_family(ui::theme::APP_MONO_FONT_FAMILY)
                            .text_xs()
                            .text_color(subtle),
                    )
                    .child(shortcuts(root)),
            );
        }
        let minimum_asset_width = if balances.is_empty() {
            0.0
        } else if selected || balances.len() == 1 {
            TILE_WIDTH
        } else {
            TILE_WIDTH + TILE_GAP + MORE_WIDTH
        };
        let mut assets = div()
            .debug_selector(|| format!("public-assets-{uuid}"))
            .w(dimension(width.max(minimum_asset_width)))
            .min_w_0()
            .flex_none()
            .flex()
            .items_center()
            .gap(dimension(TILE_GAP))
            .when(selected, gpui::Styled::flex_wrap);
        if balances.is_empty() {
            let fetched = self
                .public_balance_snapshot
                .as_ref()
                .filter(|snapshot| snapshot.chain_id == self.selected_chain)
                .is_some_and(|snapshot| {
                    snapshot.accounts.iter().any(|entry| {
                        entry.account.public_account_uuid == account.public_account_uuid
                            && entry.account.status == account.status
                    })
                });
            let chain_name = self.public_selected_chain_name();
            assets = assets.child(
                app_muted_text(if fetched {
                    format!("No balances on {chain_name}")
                } else {
                    "Balances not fetched".into()
                })
                .text_xs()
                .text_color(subtle)
                .whitespace_normal(),
            );
        } else {
            let shown = if selected {
                balances.len()
            } else {
                tiles_that_fit(width, balances.len())
            };
            for (index, entry) in balances.iter().take(shown).enumerate() {
                assets =
                    assets.child(self.render_public_asset_tile(root, account, entry, index, cx));
            }
            if shown < balances.len() {
                let hidden = public_balances_usd_total(
                    &balances[shown..],
                    self.selected_chain,
                    Some(&self.public_broadcaster_anchor_cache),
                );
                let select_root = root.clone();
                let uuid = uuid.clone();
                assets = assets.child(
                    app_button_base(SharedString::from(format!("public-more-assets-{uuid}")))
                        .debug_selector(|| format!("public-more-assets-{uuid}"))
                        .outline()
                        .w(dimension(MORE_WIDTH))
                        .h(dimension(TILE_HEIGHT))
                        .px_0()
                        .py_0()
                        .border_dashed()
                        .text_color(gpui::rgb(ui::theme::TEXT_MUTED))
                        .accessibility_label(format!("Show all {} assets", balances.len()))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .line_height(dimension(16.0))
                                        .child(format!("+{}", balances.len() - shown)),
                                )
                                .when_some(hidden.value.filter(|_| pricing), |column, value| {
                                    column.child(
                                        div()
                                            .text_size(dimension(11.0))
                                            .line_height(dimension(14.0))
                                            .text_color(subtle)
                                            .child(public_balance_compact_usd(value)),
                                    )
                                }),
                        )
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            select_root.update(cx, |root, cx| {
                                root.select_public_account_row(Some(uuid.clone()), window, cx);
                            });
                        }),
                );
            }
        }
        let mut columns = div()
            .flex()
            .items_center()
            .gap(dimension(COLUMN_GAP))
            .child(super::identicon::render_public_account_row_identicon(
                &account.address,
            ))
            .child(identity)
            .child(assets);
        if pricing {
            columns =
                columns.child(
                    div()
                        .w(dimension(TOTAL_WIDTH))
                        .flex_none()
                        .flex()
                        .flex_col()
                        .items_end()
                        .when(!balances.is_empty(), |column| {
                            column
                                .child(
                                    app_strong_text(total.value.map_or_else(
                                        || "Unavailable".into(),
                                        format_usd_micro_value,
                                    ))
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .when(dim, |text| text.text_color(subtle)),
                                )
                                .when(total.partial, |column| {
                                    column.child(
                                        div()
                                            .text_size(dimension(11.0))
                                            .line_height(dimension(14.0))
                                            .text_color(subtle)
                                            .child("partial"),
                                    )
                                })
                        }),
                );
        }
        let select_root = root.clone();
        let selected_uuid = uuid.clone();
        let context_root = root.clone();
        let context_account = account.clone();
        let mut row = div()
            .debug_selector(|| format!("public-row-{uuid}"))
            .id(group.clone())
            .group(group)
            .w_full()
            .px(dimension(ROW_INSET))
            .py(dimension(6.0))
            .min_h(dimension(48.0))
            .when(!first, |row| {
                row.border_t_1()
                    .border_color(gpui::rgb(ui::theme::BORDER_SUBTLE))
            })
            .flex()
            .flex_col()
            .justify_center()
            .gap_1()
            .when(selected, |row| {
                row.bg(gpui::rgb(ui::theme::SELECTED_ROW_SURFACE))
            })
            .when(!selected, |row| {
                row.hover(|style| style.bg(cx.theme().list_hover))
            })
            .relative()
            .when(selected, |row| {
                let bounds = self.public_form.selected_row_bounds.clone();
                row.child(
                    canvas(
                        move |bounds_in_window, _, _| bounds.set(Some(bounds_in_window)),
                        |_, (), _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                )
            })
            .when(selected, |row| {
                row.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(dimension(3.0))
                        .bg(cx.theme().primary),
                )
            })
            .on_click(move |_, window, cx| {
                select_root.update(cx, |root, cx| {
                    root.select_public_account_row(Some(selected_uuid.clone()), window, cx);
                });
            })
            .child(columns.overflow_x_scrollbar());
        if selected {
            row = row.child(
                div()
                    .pl(dimension(IDENTICON_WIDTH + COLUMN_GAP))
                    .pb(dimension(2.0))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(self.render_public_account_action_bar(root, account, cx)),
            );
        }
        row.context_menu(move |menu, _, cx| {
            context_root
                .read(cx)
                .public_account_menu(&context_root, &context_account, menu)
        })
    }

    fn render_public_account_hints(&self, window: &Window, cx: &App) -> impl IntoElement {
        let subtle = cx.theme().muted_foreground;
        let mut hints = div().flex_none().flex().items_center().gap_3();
        let actions: [(&dyn gpui::Action, &str); 6] = [
            (&FocusPublicAccountSearch, "search"),
            (&SelectNextAccount, "account"),
            (&FocusNextAsset, "asset"),
            (&ActivateFocusedAsset, "actions"),
            (&CopySelectedAddress, "copy address"),
            (&RenameSelectedAccount, "rename"),
        ];
        for (action, label) in actions {
            if let Some(key) =
                Kbd::binding_for_action_in(action, &self.public_form.list_focus, window)
            {
                hints = hints.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            key.outline()
                                .font_family(crate::assets::KEYCAP_FONT_FAMILY)
                                .font_weight(gpui::FontWeight::NORMAL)
                                .font_features(gpui::FontFeatures(Arc::new(vec![(
                                    "case".into(),
                                    1,
                                )]))),
                        )
                        .child(
                            div()
                                .text_size(dimension(11.0))
                                .line_height(dimension(14.0))
                                .text_color(subtle)
                                .child(label),
                        ),
                );
            }
        }
        hints
    }
}

fn relative_time_label(at: std::time::SystemTime) -> String {
    let elapsed = at.elapsed().unwrap_or_default().as_secs();
    if elapsed < 60 {
        "just now".to_owned()
    } else if elapsed < 3600 {
        format!("{} min ago", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{} h ago", elapsed / 3600)
    } else {
        chrono::DateTime::<chrono::Local>::from(at)
            .format("%b %-d, %H:%M")
            .to_string()
    }
}
