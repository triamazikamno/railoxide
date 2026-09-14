use gpui::{
    Context, Entity, InteractiveElement, IntoElement, ParentElement, SharedString, Styled, Window,
    div, px, rgb,
};
use gpui_component::{
    Icon, Sizable,
    alert::Alert,
    button::ButtonVariants,
    input::InputState,
    scroll::ScrollableElement,
    tab::{Tab, TabBar},
};
use ui::controls::{app_button, app_button_base, app_input, app_muted_text, app_strong_text};
use ui::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};

use crate::assets::{RailgunActionIcon, RailgunSidebarIcon};

use super::utxo::short_hash;
use super::{WalletRoot, app_status_tag};

const BROADCASTER_CONTENT_WIDTH: gpui::Pixels = px(980.0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum BroadcasterActivityTab {
    #[default]
    Monitor,
    Favorites,
    Banned,
}

impl BroadcasterActivityTab {
    const ALL: [Self; 3] = [Self::Monitor, Self::Favorites, Self::Banned];

    const fn label(self) -> &'static str {
        match self {
            Self::Monitor => "Monitor",
            Self::Favorites => "Favorites",
            Self::Banned => "Banned",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BroadcasterPreferenceListKind {
    Favorite,
    Banned,
}

impl BroadcasterPreferenceListKind {
    const fn tab(self) -> BroadcasterActivityTab {
        match self {
            Self::Favorite => BroadcasterActivityTab::Favorites,
            Self::Banned => BroadcasterActivityTab::Banned,
        }
    }

    const fn add_label(self) -> &'static str {
        match self {
            Self::Favorite => "Add favorite",
            Self::Banned => "Ban broadcaster",
        }
    }

    const fn empty_label(self) -> &'static str {
        match self {
            Self::Favorite => "No favorite broadcasters saved yet.",
            Self::Banned => "No banned broadcasters saved yet.",
        }
    }

    const fn input(self, root: &WalletRoot) -> &Entity<InputState> {
        match self {
            Self::Favorite => &root.favorite_broadcaster_input,
            Self::Banned => &root.banned_broadcaster_input,
        }
    }
}

impl WalletRoot {
    pub(super) fn render_broadcaster_view(&self, root: &Entity<Self>) -> impl IntoElement {
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .child(Self::render_broadcaster_header())
            .child(self.render_broadcaster_tabs(root))
            .children(self.broadcaster_preference_error.as_ref().map(|message| {
                div().flex_none().px(px(14.0)).pt(px(10.0)).child(
                    Alert::error("wallet-broadcaster-preference-error", message.to_string())
                        .small(),
                )
            }))
            .child(match self.active_broadcaster_tab {
                BroadcasterActivityTab::Monitor => div()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .p(px(12.0))
                    .child(self.monitor.clone())
                    .into_any_element(),
                BroadcasterActivityTab::Favorites => self
                    .render_broadcaster_preference_list(
                        root,
                        BroadcasterPreferenceListKind::Favorite,
                    )
                    .into_any_element(),
                BroadcasterActivityTab::Banned => self
                    .render_broadcaster_preference_list(root, BroadcasterPreferenceListKind::Banned)
                    .into_any_element(),
            })
    }

    fn render_broadcaster_header() -> gpui::Div {
        div()
            .flex_none()
            .px(px(16.0))
            .pt(px(16.0))
            .pb(px(8.0))
            .flex()
            .items_center()
            .gap_2()
            .child(
                Icon::new(RailgunSidebarIcon::Broadcaster)
                    .size_5()
                    .text_color(rgb(theme::PRIMARY)),
            )
            .child(app_strong_text("Broadcasters").text_size(px(20.0)))
    }

    fn render_broadcaster_tabs(&self, root: &Entity<Self>) -> impl IntoElement {
        let selected_index = BroadcasterActivityTab::ALL
            .iter()
            .position(|tab| *tab == self.active_broadcaster_tab)
            .unwrap_or(0);
        let tab_root = root.clone();

        TabBar::new("wallet-broadcaster-tabs")
            .underline()
            .w_full()
            .flex_none()
            .px(px(14.0))
            .selected_index(selected_index)
            .on_click(move |index, _window, cx| {
                let Some(tab) = BroadcasterActivityTab::ALL.get(*index).copied() else {
                    return;
                };
                tab_root.update(cx, |root, cx| {
                    root.active_broadcaster_tab = tab;
                    root.broadcaster_preference_error = None;
                    cx.notify();
                });
            })
            .children(
                BroadcasterActivityTab::ALL
                    .into_iter()
                    .map(|tab| Tab::new().min_w(px(96.0)).label(tab.label())),
            )
    }

    fn render_broadcaster_preference_list(
        &self,
        root: &Entity<Self>,
        kind: BroadcasterPreferenceListKind,
    ) -> gpui::AnyElement {
        let input = kind.input(self);
        let add_root = root.clone();
        let entries = match kind {
            BroadcasterPreferenceListKind::Favorite => &self.broadcaster_preferences.favorites,
            BroadcasterPreferenceListKind::Banned => &self.broadcaster_preferences.banned,
        };

        let mut list = div().w_full().flex().flex_col().gap_2();
        if entries.is_empty() {
            list = list.child(broadcaster_preference_empty_state(kind));
        } else {
            for (ix, entry) in entries.iter().enumerate() {
                list = list.child(Self::render_broadcaster_preference_row(
                    root,
                    kind,
                    ix,
                    &entry.address,
                ));
            }
        }

        div()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_y_scrollbar()
            .child(
                div()
                    .w(BROADCASTER_CONTENT_WIDTH)
                    .max_w_full()
                    .mx_auto()
                    .p(px(16.0))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().min_w(px(0.0)).child(app_input(input)))
                            .child(
                                app_button(
                                    SharedString::from(format!(
                                        "wallet-broadcaster-preference-add-{kind:?}"
                                    )),
                                    kind.add_label(),
                                )
                                .primary()
                                .small()
                                .on_click(
                                    move |_event, window, cx| {
                                        add_root.update(cx, |root, cx| {
                                            root.add_broadcaster_preference_from_input(
                                                kind, window, cx,
                                            );
                                        });
                                    },
                                ),
                            ),
                    )
                    .child(list),
            )
            .into_any_element()
    }

    fn render_broadcaster_preference_row(
        root: &Entity<Self>,
        kind: BroadcasterPreferenceListKind,
        ix: usize,
        address: &str,
    ) -> impl IntoElement {
        let remove_root = root.clone();
        let address = address.to_owned();
        let display_address = short_hash(&address);
        let color = match kind {
            BroadcasterPreferenceListKind::Favorite => theme::WARNING,
            BroadcasterPreferenceListKind::Banned => theme::DANGER,
        };

        div()
            .id(SharedString::from(format!(
                "wallet-broadcaster-preference-row-{kind:?}-{ix}"
            )))
            .w_full()
            .flex()
            .items_center()
            .gap_3()
            .px(px(12.0))
            .py(px(10.0))
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .child(app_status_tag(
                match kind {
                    BroadcasterPreferenceListKind::Favorite => "Favorite",
                    BroadcasterPreferenceListKind::Banned => "Banned",
                },
                color,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .font_family(APP_MONO_FONT_FAMILY)
                    .text_size(APP_TEXT_SIZE)
                    .text_color(rgb(theme::TEXT))
                    .truncate()
                    .child(display_address),
            )
            .child(
                app_button_base(SharedString::from(format!(
                    "wallet-broadcaster-preference-remove-{kind:?}-{ix}"
                )))
                .danger()
                .ghost()
                .xsmall()
                .accessibility_label("Remove broadcaster")
                .tooltip("Remove broadcaster")
                .icon(Icon::new(RailgunActionIcon::Trash2))
                .on_click(move |_event, _window, cx| {
                    let address = address.clone();
                    remove_root.update(cx, |root, cx| {
                        root.remove_broadcaster_preference(kind, &address, cx);
                    });
                }),
            )
    }

    pub(super) fn add_broadcaster_preference_from_input(
        &mut self,
        kind: BroadcasterPreferenceListKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let input = kind.input(self).clone();
        let address = input.read(cx).value().trim().to_owned();
        let saved = match kind {
            BroadcasterPreferenceListKind::Favorite => self.add_favorite_broadcaster(&address, cx),
            BroadcasterPreferenceListKind::Banned => self.add_banned_broadcaster(&address, cx),
        };
        if saved {
            self.active_broadcaster_tab = kind.tab();
            input.update(cx, |input, cx| input.set_value("", window, cx));
            cx.notify();
        }
    }

    fn remove_broadcaster_preference(
        &mut self,
        kind: BroadcasterPreferenceListKind,
        address: &str,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            BroadcasterPreferenceListKind::Favorite => {
                self.remove_favorite_broadcaster(address, cx);
            }
            BroadcasterPreferenceListKind::Banned => {
                self.remove_banned_broadcaster(address, cx);
            }
        }
    }
}

fn broadcaster_preference_empty_state(kind: BroadcasterPreferenceListKind) -> gpui::Div {
    div()
        .w_full()
        .px(px(12.0))
        .py(px(14.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE))
        .child(app_muted_text(kind.empty_label()))
}
