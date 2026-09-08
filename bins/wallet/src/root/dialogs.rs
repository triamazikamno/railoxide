use gpui::{App, Entity, IntoElement, ParentElement, Pixels, Styled, div, px, rgb};
use gpui_component::{
    Disableable, Icon, Selectable, Sizable,
    button::{Button, ButtonGroup},
    checkbox::Checkbox,
    list::List,
};
use ui::controls::app_input;
use ui::theme;

use crate::assets::{GROUP_ICON_PATH, LIST_ICON_PATH};

use super::WalletRoot;
use super::broadcaster_picker::{
    BROADCASTER_PICKER_LIST_BOTTOM_PADDING, BROADCASTER_PICKER_LIST_HORIZONTAL_PADDING,
    BROADCASTER_PICKER_LIST_TOP_PADDING, BROADCASTER_PICKER_MIN_LIST_HEIGHT,
    BroadcasterPickerContent, BroadcasterPickerDialogSnapshot, BroadcasterPickerViewMode,
    render_broadcaster_picker_header,
};
use super::private_action::delivery_element_id;

#[derive(Clone, Copy)]
pub(super) enum PublicAccountDialogKind {
    Derive,
    Import,
    EditLabel,
}

impl PublicAccountDialogKind {
    pub(super) const fn title(self) -> &'static str {
        match self {
            Self::Derive => "Derive from private",
            Self::Import => "Import private key",
            Self::EditLabel => "Edit account label",
        }
    }
}

pub(super) fn render_broadcaster_picker_dialog_content(
    root: &Entity<WalletRoot>,
    content_height: Pixels,
    cx: &mut App,
) -> gpui::AnyElement {
    let Some(snapshot) = root.read(cx).broadcaster_picker_dialog_snapshot(cx) else {
        return div().into_any_element();
    };
    let BroadcasterPickerDialogSnapshot {
        query_input,
        list,
        scroll_indicator,
        entries,
        empty_message,
        generating,
        query,
        filtered_count,
        total_count,
        show_all_broadcasters,
        fee_status_popover_open,
        view_mode,
        selected_address,
        expanded_groups,
        collapsed_selected_children,
        kind,
        key,
    } = snapshot;
    list.update(cx, |list, cx| {
        let content = BroadcasterPickerContent {
            entries,
            empty_message,
            generating,
            show_all_broadcasters,
            query,
            selected_address,
            view_mode,
            expanded_groups,
            collapsed_selected_children,
        };
        if list.delegate_mut().set_content(content, cx) {
            cx.notify();
        }
    });

    let toggle_root = root.clone();
    let view_root = root.clone();
    div()
        .w_full()
        .h(content_height)
        .min_h(px(220.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div().flex().items_center().gap_3().child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(app_input(&query_input).small().disabled(generating)),
            ),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    Checkbox::new(delivery_element_id(key, kind, "show-all-broadcasters"))
                        .label("Allow out-of-range fees")
                        .checked(show_all_broadcasters)
                        .xsmall()
                        .disabled(generating)
                        .on_click(move |checked, _window, cx| {
                            let checked = *checked;
                            toggle_root.update(cx, |root, cx| {
                                root.set_allow_suspicious_broadcasters(kind, key, checked, cx);
                            });
                        }),
                )
                .child(
                    ButtonGroup::new(delivery_element_id(
                        key,
                        kind,
                        "broadcaster-picker-view-mode",
                    ))
                    .child(
                        Button::new(delivery_element_id(
                            key,
                            kind,
                            "broadcaster-picker-view-grouped",
                        ))
                        .icon(Icon::empty().path(GROUP_ICON_PATH))
                        .selected(view_mode == BroadcasterPickerViewMode::Grouped)
                        .accessibility_label("Grouped view")
                        .tooltip("Grouped view"),
                    )
                    .child(
                        Button::new(delivery_element_id(
                            key,
                            kind,
                            "broadcaster-picker-view-list",
                        ))
                        .icon(Icon::empty().path(LIST_ICON_PATH))
                        .selected(view_mode == BroadcasterPickerViewMode::List)
                        .accessibility_label("List view")
                        .tooltip("List view"),
                    )
                    .compact()
                    .outline()
                    .small()
                    .disabled(generating)
                    .on_click(move |selected, _window, cx| {
                        let Some(index) = selected.first() else {
                            return;
                        };
                        let view_mode = if *index == 0 {
                            BroadcasterPickerViewMode::Grouped
                        } else {
                            BroadcasterPickerViewMode::List
                        };
                        view_root.update(cx, |root, cx| {
                            root.set_broadcaster_picker_view_mode(view_mode, cx);
                        });
                    }),
                ),
        )
        .child(render_broadcaster_picker_header(
            root,
            &query_input,
            filtered_count,
            total_count,
            fee_status_popover_open,
        ))
        .child(
            div()
                .relative()
                .flex_1()
                .min_h(BROADCASTER_PICKER_MIN_LIST_HEIGHT)
                .min_w(px(0.0))
                .w_full()
                .child(
                    List::new(&list)
                        .px(BROADCASTER_PICKER_LIST_HORIZONTAL_PADDING)
                        .pt(BROADCASTER_PICKER_LIST_TOP_PADDING)
                        .pb(BROADCASTER_PICKER_LIST_BOTTOM_PADDING)
                        .size_full()
                        .bg(rgb(theme::SURFACE)),
                )
                .child(scroll_indicator),
        )
        .into_any_element()
}
