//! Recipient input and searchable suggestions shared by native and browser forms.
use std::rc::Rc;

use gpui::{
    App, Bounds, ElementId, Entity, Focusable as _, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Pixels, RenderOnce, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled, Window, anchored, canvas, deferred, div, point,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, Icon, Selectable as _, Sizable,
    button::Button,
    input::{Escape, InputState},
    scroll::ScrollableElement as _,
};

use crate::{
    controls::{app_muted_text, app_strong_text, recipient_book_button, recipient_input},
    theme,
};

#[derive(Clone)]
pub struct RecipientSuggestion {
    id: SharedString,
    label: SharedString,
    address: SharedString,
    display_address: SharedString,
    account: bool,
}

impl RecipientSuggestion {
    #[must_use]
    pub fn new(
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        address: impl Into<SharedString>,
    ) -> Self {
        let address = address.into();
        Self {
            id: id.into(),
            label: label.into(),
            display_address: address.clone(),
            address,
            account: false,
        }
    }

    #[must_use]
    pub const fn account(mut self, account: bool) -> Self {
        self.account = account;
        self
    }

    #[must_use]
    pub fn display_address(mut self, address: impl Into<SharedString>) -> Self {
        self.display_address = address.into();
        self
    }

    #[must_use]
    pub const fn id(&self) -> &SharedString {
        &self.id
    }

    #[must_use]
    pub const fn address(&self) -> &SharedString {
        &self.address
    }

    #[must_use]
    pub fn matches(&self, query: &str) -> bool {
        matches_search(&self.label, &self.address, query)
    }
}

#[must_use]
pub fn matches_search(label: &str, address: &str, query: &str) -> bool {
    let query = query.trim().to_ascii_lowercase();
    query.is_empty()
        || label.to_ascii_lowercase().contains(&query)
        || address.to_ascii_lowercase().contains(&query)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecipientSuggestionDirection {
    Previous,
    Next,
}

#[must_use]
pub const fn suggestion_index_after_move(
    current: Option<usize>,
    len: usize,
    direction: RecipientSuggestionDirection,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (current, direction) {
        (Some(index), RecipientSuggestionDirection::Next) => (index + 1) % len,
        (Some(0) | None, RecipientSuggestionDirection::Previous) => len - 1,
        (Some(index), RecipientSuggestionDirection::Previous) => index.saturating_sub(1),
        (None, RecipientSuggestionDirection::Next) => 0,
    })
}

pub enum RecipientPickerEvent {
    Toggle,
    Dismiss,
    Move(RecipientSuggestionDirection),
    Select(SharedString),
}

type Listener = Rc<dyn Fn(&RecipientPickerEvent, &mut Window, &mut App)>;

#[derive(Default)]
struct LayoutState {
    bounds: Option<Bounds<Pixels>>,
}

/// Controlled picker: the form owns selection and protocol validation; this control owns
/// input geometry, filtered rows, keyboard routing, and the full-width anchored list.
#[derive(IntoElement)]
pub struct RecipientPicker {
    id: ElementId,
    input: Entity<InputState>,
    options: Vec<RecipientSuggestion>,
    query: String,
    open: bool,
    selected_index: Option<usize>,
    scroll: ScrollHandle,
    disabled: bool,
    action: Option<Button>,
    on_event: Listener,
}

impl RecipientPicker {
    #[must_use]
    pub fn new(
        id: impl Into<ElementId>,
        input: &Entity<InputState>,
        options: Vec<RecipientSuggestion>,
        on_event: impl Fn(&RecipientPickerEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            input: input.clone(),
            options,
            query: String::new(),
            open: false,
            selected_index: None,
            scroll: ScrollHandle::new(),
            disabled: false,
            action: None,
            on_event: Rc::new(on_event),
        }
    }

    #[must_use]
    pub fn query(mut self, query: impl Into<String>) -> Self {
        self.query = query.into();
        self
    }

    #[must_use]
    pub fn suggestions(
        mut self,
        open: bool,
        selected_index: Option<usize>,
        scroll: &ScrollHandle,
    ) -> Self {
        self.open = open;
        self.selected_index = selected_index;
        self.scroll = scroll.clone();
        self
    }

    /// Replace the address-book action when a native form can save a new recipient.
    #[must_use]
    pub fn action(mut self, action: Button) -> Self {
        self.action = Some(action);
        self
    }
}

impl Disableable for RecipientPicker {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl RenderOnce for RecipientPicker {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state((self.id.clone(), "layout"), cx, |_, _| {
            LayoutState::default()
        });
        let bounds = state.read(cx).bounds;
        let open = self.open && !self.disabled && self.action.is_none();
        let outside = self.on_event.clone();
        let escape = self.on_event.clone();
        let keyboard = self.on_event.clone();
        let toggle = self.on_event.clone();
        let focus_input = self.input.clone();
        let action = self.action.unwrap_or_else(|| {
            recipient_book_button("address-book")
                .selected(open)
                .disabled(self.disabled || self.options.is_empty())
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    focus_input.read(cx).focus_handle(cx).focus(window, cx);
                    toggle(&RecipientPickerEvent::Toggle, window, cx);
                })
        });
        // Keep suggestions below the field, scrolling the list to fit shorter viewports.
        // Account for the menu's padding/border and the window-edge margin.
        let max_height = bounds.map_or(px(220.0), |bounds| {
            (window.viewport_size().height - bounds.bottom() - px(26.0)).clamp(px(0.0), px(220.0))
        });
        let menu = open.then(|| {
            let mut list = div()
                .id("suggestions")
                .max_h(max_height)
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .flex()
                .flex_col()
                .gap_1();
            let mut count = 0;
            for (index, option) in self
                .options
                .into_iter()
                .filter(|option| option.matches(&self.query))
                .enumerate()
            {
                count += 1;
                let select = self.on_event.clone();
                let id = option.id.clone();
                list = list.child(
                    div()
                        .id((
                            ElementId::from(option.id),
                            if option.account { "account" } else { "book" },
                        ))
                        .w_full()
                        .min_w_0()
                        .p(px(8.0))
                        .flex()
                        .flex_col()
                        .flex_none()
                        .gap_1()
                        .rounded_sm()
                        .when(self.selected_index == Some(index), |this| {
                            this.bg(rgb(theme::SURFACE_HOVER))
                        })
                        .hover(|this| this.bg(rgb(theme::SURFACE_HOVER)))
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            cx.stop_propagation();
                            select(&RecipientPickerEvent::Select(id.clone()), window, cx);
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(app_strong_text(option.label).flex_1().min_w_0().truncate())
                                .child(
                                    if option.account {
                                        Icon::empty().path(crate::icons::wallet_icon_path())
                                    } else {
                                        Icon::empty().path("ui/icons/book-user.svg")
                                    }
                                    .small()
                                    .text_color(rgb(theme::TEXT_MUTED)),
                                ),
                        )
                        .child(
                            app_muted_text(option.display_address)
                                .text_size(px(11.0))
                                .w_full()
                                .truncate(),
                        ),
                );
            }
            div()
                .w_full()
                .p(px(8.0))
                .flex()
                .flex_col()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER))
                .bg(rgb(theme::POPOVER_BG))
                .overflow_hidden()
                .occlude()
                .when(count == 0, |this| {
                    this.child(app_muted_text("No matching recipients"))
                })
                .when(count > 0, |this| {
                    this.child(list).vertical_scrollbar(&self.scroll)
                })
        });
        div()
            .id(self.id)
            .relative()
            .w_full()
            .on_mouse_down_out(move |_, window, cx| {
                outside(&RecipientPickerEvent::Dismiss, window, cx);
            })
            .on_action(move |_: &Escape, window, cx| {
                if open {
                    escape(&RecipientPickerEvent::Dismiss, window, cx);
                } else {
                    cx.propagate();
                }
            })
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                let event = match event.keystroke.key.as_str() {
                    "down" => RecipientPickerEvent::Move(RecipientSuggestionDirection::Next),
                    "up" => RecipientPickerEvent::Move(RecipientSuggestionDirection::Previous),
                    "escape" | "tab" if open => RecipientPickerEvent::Dismiss,
                    _ => return,
                };
                let dismiss = matches!(event, RecipientPickerEvent::Dismiss);
                keyboard(&event, window, cx);
                if !dismiss {
                    cx.stop_propagation();
                }
            })
            .child(
                div()
                    .relative()
                    .w_full()
                    .child(recipient_input(&self.input, action).disabled(self.disabled))
                    .child(
                        canvas(
                            move |bounds, window, cx| {
                                let changed = state.update(cx, |state, _| {
                                    let changed = state.bounds != Some(bounds);
                                    state.bounds = Some(bounds);
                                    changed
                                });
                                if changed {
                                    window.request_animation_frame();
                                }
                            },
                            |_, (), _, _| {},
                        )
                        // Measure the input frame, not the static position after its contents.
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    ),
            )
            .children(bounds.zip(menu).map(|(bounds, menu)| {
                deferred(
                    anchored()
                        .position(point(bounds.left(), bounds.bottom()))
                        .snap_to_window_with_margin(px(8.0))
                        .child(div().w(bounds.size.width).child(menu)),
                )
                .with_priority(gpui_kit::base::POPUP_PRIORITY)
            }))
    }
}
