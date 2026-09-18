use gpui::{
    AnyElement, App, Div, ElementId, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, SharedString, Styled, Window, div, prelude::FluentBuilder as _, px, relative,
    rgb,
};
use gpui_component::input::{
    Copy, Cut, DeleteToBeginningOfLine, DeleteToEndOfLine, DeleteToNextWordEnd,
    DeleteToPreviousWordStart, Input, InputState, MoveToEnd, MoveToNextWord, MoveToPreviousWord,
    MoveToStart, SelectToEnd, SelectToNextWordEnd, SelectToPreviousWordStart, SelectToStart,
};
use gpui_component::{
    Disableable, Icon, IconName, IndexPath, Selectable, Sizable,
    button::{Button, ButtonGroup, ButtonVariants},
    select::{SearchableVec, SelectDelegate, SelectItem},
};

use crate::theme::{self, APP_TEXT_LINE_HEIGHT, APP_TEXT_SIZE};

/// Searchable items whose custom rows fill the menu width.
///
/// Uses the delegate hook to bypass the default `SelectItem` content-sized wrapper.
pub struct FullWidthSelectItems<T: SelectItem + 'static>(SearchableVec<T>);

impl<T: SelectItem + 'static> FullWidthSelectItems<T> {
    #[must_use]
    pub fn new(items: Vec<T>) -> Self {
        Self(SearchableVec::new(items))
    }
}

impl<T: SelectItem + 'static> SelectDelegate for FullWidthSelectItems<T> {
    type Item = T;

    fn items_count(&self, section: usize) -> usize {
        self.0.items_count(section)
    }

    fn item(&self, ix: IndexPath) -> Option<&Self::Item> {
        self.0.item(ix)
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SelectItem<Value = V>,
        V: PartialEq,
    {
        self.0.position(value)
    }

    fn perform_search(&mut self, query: &str, window: &mut Window, cx: &mut App) -> gpui::Task<()> {
        self.0.perform_search(query, window, cx)
    }

    fn render_item(
        &self,
        _: IndexPath,
        item: &Self::Item,
        checked: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        Some(
            div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap_1()
                .child(item.render(window, cx))
                .child(
                    Icon::new(IconName::Check)
                        .xsmall()
                        .when(!checked, Styled::invisible),
                )
                .into_any_element(),
        )
    }
}

#[must_use]
pub fn app_input(state: &Entity<InputState>) -> Input {
    Input::new(state)
        .w_full()
        .px(px(8.0))
        .bg(rgb(theme::SURFACE))
}

/// Recipient field shared by Unshield and browser Send, with integrated trailing actions.
#[must_use]
pub fn recipient_input(state: &Entity<InputState>, actions: impl IntoElement) -> Input {
    app_input(state)
        .px_3()
        .aria_label("Recipient")
        .suffix(actions)
}

#[must_use]
pub fn recipient_book_button(id: impl Into<ElementId>) -> Button {
    app_button_base(id)
        .icon(Icon::empty().path("ui/icons/book-user.svg"))
        .outline()
        .small()
        .compact()
        .accessibility_label("Select recipient")
        .tooltip("Select recipient")
}

#[must_use]
pub fn app_masked_input(state: &Entity<InputState>, disabled: bool) -> Div {
    div()
        .w_full()
        .capture_action::<Copy>(|_, _, cx| cx.stop_propagation())
        .capture_action::<Cut>(|_, _, cx| cx.stop_propagation())
        .capture_action::<MoveToPreviousWord>(|_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(Box::new(MoveToStart), cx);
        })
        .capture_action::<MoveToNextWord>(|_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(Box::new(MoveToEnd), cx);
        })
        .capture_action::<SelectToPreviousWordStart>(|_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(Box::new(SelectToStart), cx);
        })
        .capture_action::<SelectToNextWordEnd>(|_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(Box::new(SelectToEnd), cx);
        })
        .capture_action::<DeleteToPreviousWordStart>(|_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(Box::new(DeleteToBeginningOfLine), cx);
        })
        .capture_action::<DeleteToNextWordEnd>(|_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(Box::new(DeleteToEndOfLine), cx);
        })
        .child(
            app_input(state)
                .role(gpui::accesskit::Role::PasswordInput)
                .disabled(disabled),
        )
}

#[must_use]
pub fn app_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    let label: SharedString = label.into();
    app_button_base(id)
        .accessibility_label(label.clone())
        .child(app_button_label(label))
}

#[must_use]
pub fn amount_max_button(id: impl Into<ElementId>, amount: Option<String>) -> Button {
    app_button(
        id,
        amount.map_or_else(|| "Max".to_owned(), |amount| format!("Max: {amount}")),
    )
    .link()
    .xsmall()
    .compact()
}

#[must_use]
pub fn app_button_base(id: impl Into<ElementId>) -> Button {
    Button::new(id).secondary()
}

/// Shared Public action mode control. The caller owns the selected mode.
#[must_use]
pub fn public_action_mode_group(
    id: impl Into<ElementId>,
    shield_selected: bool,
    disabled: bool,
    on_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> ButtonGroup {
    action_mode_group(
        id,
        shield_selected,
        disabled,
        [
            ("shield", "Shield", "ui/icons/shield.svg"),
            ("send", "Send", "ui/icons/arrow-big-right-dash.svg"),
        ],
        on_change,
    )
}

/// Two action modes with the same selection and keyboard behavior on both surfaces.
#[must_use]
pub fn action_mode_group(
    id: impl Into<ElementId>,
    first_selected: bool,
    disabled: bool,
    choices: [(&'static str, &'static str, &'static str); 2],
    on_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> ButtonGroup {
    let on_change = std::rc::Rc::new(on_change);
    let segment = |id, label, icon, shield| {
        let on_change = on_change.clone();
        let selected = shield == first_selected;
        app_button(id, label)
            .flex_1()
            .min_w_0()
            .icon(Icon::empty().path(icon).small())
            .selected(selected)
            .when(selected, ButtonVariants::primary)
            .on_click(move |_, window, cx| on_change(&shield, window, cx))
    };
    ButtonGroup::new(id)
        .w_full()
        .outline()
        .disabled(disabled)
        .child(segment(choices[0].0, choices[0].1, choices[0].2, true))
        .child(
            segment(choices[1].0, choices[1].1, choices[1].2, false)
                // The selected segment owns the shared one-pixel border, as on desktop.
                .when(!first_selected, |button| button.border_l_1().ml(-px(1.0))),
        )
}

#[must_use]
pub fn app_segment_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    disabled: bool,
    accessory: Option<AnyElement>,
) -> Button {
    let label: SharedString = label.into();
    app_button_base(id)
        .accessibility_label(label.clone())
        .selected(selected)
        .disabled(disabled)
        .when(!disabled, |button| {
            button
                .bg(if selected {
                    rgb(theme::SURFACE_HOVER)
                } else {
                    gpui::transparent_black().into()
                })
                .text_color(rgb(if selected {
                    theme::PRIMARY
                } else {
                    theme::TEXT_MUTED
                }))
        })
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .gap_1()
                .child(app_button_label(label))
                .children(accessory),
        )
}

#[must_use]
pub fn app_inline_control_row(label: impl Into<SharedString>, control: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(div().min_w(px(0.0)).child(app_muted_text(label)))
        .child(div().flex_none().child(control))
}

/// Inherits the control's font size while keeping shared proportional leading.
#[must_use]
pub fn app_button_label(label: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .font_weight(FontWeight::LIGHT)
        .line_height(relative(APP_TEXT_LINE_HEIGHT))
        .child(label.into())
}

/// Body text owns its font size and line height; control labels inherit their size.
#[must_use]
pub fn app_text(label: impl Into<SharedString>) -> Div {
    div()
        .text_size(APP_TEXT_SIZE)
        .line_height(relative(APP_TEXT_LINE_HEIGHT))
        .child(label.into())
}

#[must_use]
pub fn app_muted_text(label: impl Into<SharedString>) -> Div {
    app_text(label).text_color(rgb(theme::TEXT_MUTED))
}

#[must_use]
pub fn app_strong_text(label: impl Into<SharedString>) -> Div {
    app_text(label)
        .text_color(rgb(theme::TEXT))
        .font_weight(FontWeight::MEDIUM)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{
        AppContext as _, ClipboardItem, Context, Entity, Focusable as _, IntoElement,
        ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext, Window, div,
    };
    use gpui_component::input::{
        Copy, Cut, Delete, InputState, MoveToPreviousWord, SelectAll, SelectToPreviousWordStart,
    };

    use super::app_masked_input;

    struct MaskedInputProbe {
        input: Entity<InputState>,
    }

    impl Render for MaskedInputProbe {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().w_full().child(app_masked_input(&self.input, false))
        }
    }

    #[gpui::test]
    fn masked_input_blocks_secret_export_and_word_boundaries(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let input_slot = Rc::new(RefCell::new(None));
        let input_slot_for_window = Rc::clone(&input_slot);
        let window = cx.add_window(|window, cx| {
            let input = cx.new(|cx| InputState::new(window, cx).masked(true));
            input.update(cx, |input, cx| input.set_value("secret value", window, cx));
            *input_slot_for_window.borrow_mut() = Some(input.clone());
            let probe = cx.new(|_| MaskedInputProbe { input });
            gpui_component::Root::new(probe, window, cx)
        });
        let input = input_slot.borrow_mut().take().expect("masked input entity");
        let cx = VisualTestContext::from_window(*window, cx).into_mut();

        cx.update(|window, app| {
            input.read(app).focus_handle(app).focus(window, app);
        });
        cx.refresh().expect("refresh masked input test window");
        cx.run_until_parked();

        cx.dispatch_action(SelectAll);
        cx.write_to_clipboard(ClipboardItem::new_string("keep me".to_owned()));
        cx.dispatch_action(Copy);
        assert_eq!(
            cx.read_from_clipboard()
                .expect("clipboard item")
                .text()
                .as_deref(),
            Some("keep me")
        );
        cx.dispatch_action(Cut);
        let value = cx.update(|_, app| input.read(app).value());
        assert_eq!(value.as_ref(), "secret value");
        assert_eq!(
            cx.read_from_clipboard()
                .expect("clipboard item")
                .text()
                .as_deref(),
            Some("keep me")
        );

        cx.update(|window, app| {
            input.update(app, |input, cx| input.set_value("aaa bbb", window, cx));
        });
        cx.run_until_parked();
        cx.dispatch_action(MoveToPreviousWord);
        let cursor = cx.update(|_, app| input.read(app).cursor_position());
        assert_eq!(cursor.character, 0);

        cx.update(|window, app| {
            input.update(app, |input, cx| input.set_value("aaa bbb", window, cx));
        });
        cx.run_until_parked();
        cx.dispatch_action(SelectToPreviousWordStart);
        cx.dispatch_action(Delete);
        let value = cx.update(|_, app| input.read(app).value());
        assert_eq!(value.as_ref(), "");
    }
}
