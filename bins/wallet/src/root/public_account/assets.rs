use gpui::{
    App, Context, DismissEvent, Entity, Focusable, InteractiveElement, IntoElement, ParentElement,
    SharedString, Styled, Window, div, img, prelude::FluentBuilder,
};
use gpui_component::{
    ActiveTheme, Icon, IconName, Selectable,
    button::{ButtonCustomVariant, ButtonVariants},
    menu::{PopupMenu, PopupMenuItem},
    popover::Popover,
};
use gpui_kit::base::actions::{Cancel, Confirm};
use std::{cmp::Reverse, sync::Arc};
use ui::controls::app_button_base;
use wallet_ops::{PublicAssetId, PublicBalanceEntry, vault::PublicAccountMetadata};

use super::list::{TILE_HEIGHT, TILE_WIDTH, dimension};
use crate::root::{
    WalletRoot,
    public_action::PublicActionMode,
    public_balances::{
        public_asset_icon_path, public_balance_compact_usd, public_balance_tile_amount,
        public_balance_usd_value,
    },
};

impl WalletRoot {
    pub(super) fn public_account_sorted_balances(
        &self,
        account: &PublicAccountMetadata,
    ) -> Vec<PublicBalanceEntry> {
        let mut balances =
            self.public_account_visible_balances(&account.public_account_uuid, account.status);
        balances.sort_by_cached_key(|entry| {
            Reverse(
                public_balance_usd_value(
                    self.selected_chain,
                    entry.asset.id,
                    &entry.amount,
                    Some(&self.public_broadcaster_anchor_cache),
                )
                .unwrap_or_default(),
            )
        });
        balances
    }

    pub(super) fn move_public_asset_focus(
        &mut self,
        next: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(account) = self.selected_public_account() else {
            return;
        };
        let count = self.public_account_sorted_balances(account).len();
        if count == 0 {
            return;
        }
        self.public_form.focused_asset_index = Some(self.public_form.focused_asset_index.map_or(
            if next { 0 } else { count - 1 },
            |index| {
                if next {
                    (index + 1).min(count - 1)
                } else {
                    index.saturating_sub(1)
                }
            },
        ));
        self.public_form.list_focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn activate_public_focused_asset(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(account) = self.selected_public_account() else {
            return;
        };
        let Some(entry) = self
            .public_account_sorted_balances(account)
            .get(self.public_form.focused_asset_index.unwrap_or(0))
            .cloned()
        else {
            return;
        };
        self.open_public_asset_menu(
            Arc::from(account.public_account_uuid.as_str()),
            entry.asset.id,
            window,
            cx,
        );
    }

    /// Asset under keyboard focus on the selected row, by identity, so a
    /// balance refresh that reorders tiles can find it again.
    pub(in crate::root) fn public_focused_asset(&self) -> Option<PublicAssetId> {
        let account = self.selected_public_account()?;
        let index = self.public_form.focused_asset_index?;
        self.public_account_sorted_balances(account)
            .get(index)
            .map(|entry| entry.asset.id)
    }

    /// Re-points tile focus and the open asset menu after the balances under
    /// them changed. Returns true when an open menu closed because its asset
    /// is gone; the menu held keyboard focus, so the caller must return it to
    /// the list.
    pub(in crate::root) fn revalidate_public_asset_focus(
        &mut self,
        focused: Option<PublicAssetId>,
    ) -> bool {
        let balances = self
            .selected_public_account()
            .map(|account| self.public_account_sorted_balances(account))
            .unwrap_or_default();
        let position =
            |asset: PublicAssetId| balances.iter().position(|entry| entry.asset.id == asset);
        let menu_asset = self
            .public_form
            .asset_menu
            .as_ref()
            .map(|(asset, _)| *asset);
        let menu_index = menu_asset.and_then(position);
        let menu_closed = menu_asset.is_some() && menu_index.is_none();
        if menu_closed {
            self.public_form.asset_menu = None;
            self.public_form.asset_menu_subscription = None;
        }
        self.public_form.focused_asset_index = menu_index.or_else(|| focused.and_then(position));
        menu_closed
    }

    fn open_public_asset_menu(
        &mut self,
        uuid: Arc<str>,
        asset: PublicAssetId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(account) = self.public_account_for_uuid(Some(&uuid)).cloned() else {
            return;
        };
        let balances = self.public_account_sorted_balances(&account);
        let Some(index) = balances.iter().position(|entry| entry.asset.id == asset) else {
            return;
        };
        self.select_public_account_row(Some(uuid.clone()), window, cx);
        self.public_form.focused_asset_index = Some(index);
        self.public_form.list_focus.focus(window, cx);
        let root = cx.entity();
        let shield = self.selected_chain_has_railgun();
        let focus = self.public_form.list_focus.clone();
        let menu = PopupMenu::build(window, cx, move |menu, _, _| {
            let mut menu = menu.action_context(focus);
            for mode in [PublicActionMode::Shield, PublicActionMode::Send] {
                if mode == PublicActionMode::Shield && !shield {
                    continue;
                }
                let root = root.clone();
                let uuid = uuid.clone();
                let (label, icon) = if mode == PublicActionMode::Shield {
                    ("Shield…", crate::assets::RailgunActionIcon::Shield)
                } else {
                    ("Send…", crate::assets::RailgunActionIcon::Send)
                };
                menu = menu.item(PopupMenuItem::new(label).icon(Icon::new(icon)).on_click(
                    move |_, window, cx| {
                        root.update(cx, |root, cx| {
                            root.public_form.asset_menu = None;
                            // The dialog must return focus to the list, not the closing menu.
                            root.public_form.list_focus.focus(window, cx);
                            root.open_public_action_dialog(uuid.clone(), asset, window, cx);
                            root.public_form.action_mode = mode;
                            root.refresh_public_action_gas_fee_quote(mode, cx);
                            cx.notify();
                        });
                    },
                ));
            }
            menu
        });
        self.public_form.asset_menu_subscription =
            Some(
                cx.subscribe_in(&menu, window, |root, _, _: &DismissEvent, _, cx| {
                    root.public_form.asset_menu = None;
                    cx.notify();
                }),
            );
        menu.focus_handle(cx).focus(window, cx);
        self.public_form.asset_menu = Some((asset, menu));
        cx.notify();
    }

    pub(super) fn render_public_asset_tile(
        &self,
        root: &Entity<Self>,
        account: &PublicAccountMetadata,
        entry: &PublicBalanceEntry,
        index: usize,
        cx: &App,
    ) -> impl IntoElement {
        let selected = self.public_form.selected_account_uuid.as_deref()
            == Some(account.public_account_uuid.as_str());
        let focused = selected && self.public_form.focused_asset_index == Some(index);
        let asset = entry.asset.id;
        let menu = self
            .public_form
            .asset_menu
            .as_ref()
            .filter(|(open_asset, _)| selected && *open_asset == asset)
            .map(|(_, menu)| menu.clone());
        let open = menu.is_some();
        let uuid: Arc<str> = Arc::from(account.public_account_uuid.as_str());
        let id = SharedString::from(format!("public-asset-{uuid}-{asset:?}"));
        let amount = public_balance_tile_amount(&entry.amount, entry.asset.decimals);
        let usd = public_balance_usd_value(
            self.selected_chain,
            asset,
            &entry.amount,
            Some(&self.public_broadcaster_anchor_cache),
        )
        .filter(|_| self.public_chain_has_pricing())
        .map(public_balance_compact_usd);
        let icon = public_asset_icon_path(
            self.selected_chain,
            asset,
            Some(&self.effective_token_registry),
        );
        let subtle = cx.theme().muted_foreground;
        let dust = amount.starts_with('<');
        // Tiles take the page color: flush with an unselected row, one step
        // above the recessed selected row.
        let variant = ButtonCustomVariant::new(cx)
            .color(if open {
                cx.theme().secondary
            } else {
                gpui::rgb(ui::theme::BACKGROUND).into()
            })
            .hover(cx.theme().secondary)
            .active(cx.theme().secondary)
            .foreground(cx.theme().foreground);
        let trigger = app_button_base(id.clone())
            .custom(variant)
            .selected(open)
            .w(dimension(TILE_WIDTH))
            .h(dimension(TILE_HEIGHT))
            .pl(dimension(6.0))
            .pr(dimension(5.0))
            .py_0()
            .tab_stop(true)
            .border_1()
            .border_color(if focused || open {
                cx.theme().primary
            } else {
                gpui::rgb(ui::theme::BORDER_SUBTLE).into()
            })
            .when(!focused && !open, |button| {
                button.hover(|style| style.border_color(cx.theme().border))
            })
            .accessibility_label(format!("{} {} actions", entry.asset.symbol, amount))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(dimension(6.0))
                    .child(asset_disc(icon, &entry.asset.symbol, cx))
                    .child(
                        div()
                            .max_w(dimension(48.0))
                            .truncate()
                            .text_size(dimension(13.0))
                            .line_height(dimension(16.0))
                            .text_color(cx.theme().foreground)
                            .child(entry.asset.symbol.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .items_end()
                            .child(
                                div()
                                    .text_size(dimension(13.0))
                                    .line_height(dimension(16.0))
                                    .font_weight(if dust {
                                        gpui::FontWeight::NORMAL
                                    } else {
                                        gpui::FontWeight::MEDIUM
                                    })
                                    .text_color(if dust { subtle } else { cx.theme().foreground })
                                    .whitespace_nowrap()
                                    .child(amount),
                            )
                            .when_some(usd, |column, usd| {
                                column.child(
                                    div()
                                        .text_size(dimension(11.0))
                                        .line_height(dimension(14.0))
                                        .text_color(subtle)
                                        .child(usd),
                                )
                            }),
                    )
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .size(dimension(12.0))
                            .text_color(if open { cx.theme().foreground } else { subtle }),
                    ),
            );
        let open_root = root.clone();
        let popover = Popover::new(id.clone())
            .appearance(false)
            .overlay_closable(false)
            .open(open)
            .trigger(trigger)
            .on_open_change(move |open, window, cx| {
                cx.stop_propagation();
                open_root.update(cx, |root, cx| {
                    if *open {
                        root.open_public_asset_menu(uuid.clone(), asset, window, cx);
                    } else {
                        root.public_form.asset_menu = None;
                        cx.notify();
                    }
                });
            });
        let popover = if let Some(menu) = menu {
            popover
                .track_focus(&menu.focus_handle(cx))
                .content(move |_, _, cx| {
                    let focus = menu.focus_handle(cx);
                    div()
                        .capture_action(move |_: &Confirm, window, cx| {
                            let focus = focus.clone();
                            // Run a selected command first; otherwise Enter dismisses the menu.
                            window.defer(cx, move |window, cx| {
                                if focus.is_focused(window) {
                                    window.dispatch_action(Box::new(Cancel), cx);
                                }
                            });
                        })
                        .child(menu.clone())
                })
        } else {
            popover
        };
        div()
            .debug_selector(|| format!("public-asset-{}-{asset:?}", account.public_account_uuid))
            .id(id)
            .flex_none()
            .child(popover)
    }
}

/// 22px asset icon, or a lettered placeholder disc of the same size.
fn asset_disc(
    icon: Option<crate::assets::WalletIconSource>,
    symbol: &str,
    cx: &App,
) -> gpui::AnyElement {
    let size = dimension(22.0);
    icon.map_or_else(
        || {
            div()
                .size(size)
                .flex_none()
                .rounded_full()
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().secondary)
                .flex()
                .items_center()
                .justify_center()
                .text_size(dimension(11.0))
                .line_height(dimension(11.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child(
                    symbol
                        .chars()
                        .next()
                        .unwrap_or('?')
                        .to_uppercase()
                        .to_string(),
                )
                .into_any_element()
        },
        |path| {
            img(path)
                .size(size)
                .flex_none()
                .rounded_full()
                .into_any_element()
        },
    )
}
