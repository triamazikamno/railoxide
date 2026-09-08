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
    button::{Button, ButtonVariants},
    select::{SearchableVec, SelectDelegate, SelectItem},
};

use crate::theme::{self, APP_TEXT_SIZE};

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
pub fn app_button_base(id: impl Into<ElementId>) -> Button {
    Button::new(id).secondary()
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

#[must_use]
pub fn app_button_label(label: impl Into<SharedString>) -> Div {
    app_text(label).flex_none()
}

#[must_use]
pub fn app_text(label: impl Into<SharedString>) -> Div {
    div()
        .text_size(APP_TEXT_SIZE)
        .line_height(relative(1.0))
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
        .font_weight(FontWeight::SEMIBOLD)
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
