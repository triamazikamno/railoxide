//! Controlled gas price editor shared by desktop transaction forms and the extension.
use std::rc::Rc;

use gpui::{
    App, Entity, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div,
    relative,
};
use gpui_component::{
    ActiveTheme as _, Disableable, Icon, Sizable, Size, StyleSized,
    button::{Button, ButtonGroup, ButtonVariants},
    input::InputState,
};

use crate::{
    controls::{
        app_button_base, app_inline_control_row, app_input, app_muted_text, app_segment_button,
    },
    icons,
    theme::{APP_TEXT_LINE_HEIGHT, APP_TEXT_SIZE},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GasFeeMode {
    Auto,
    Custom,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GasFeeEditTarget {
    MaxFee,
    MaxTip,
}

/// User intent; the owner applies mode changes, seeds inputs, and requests quotes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GasFeeEditorEvent {
    Mode(GasFeeMode),
    Refresh,
    Edit(GasFeeEditTarget),
}

type GasFeeHandler = Rc<dyn Fn(&GasFeeEditorEvent, &mut Window, &mut App)>;

/// The caller retains inputs and quote state. Rendering never edits or submits them.
#[derive(IntoElement)]
pub struct GasFeeEditor {
    id: SharedString,
    max_fee: Entity<InputState>,
    max_tip: Entity<InputState>,
    mode: GasFeeMode,
    quote: Option<(String, String)>,
    refreshing: bool,
    disabled: bool,
    on_event: GasFeeHandler,
}

impl GasFeeEditor {
    pub fn new(
        id: impl Into<SharedString>,
        max_fee: &Entity<InputState>,
        max_tip: &Entity<InputState>,
        on_event: impl Fn(&GasFeeEditorEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            max_fee: max_fee.clone(),
            max_tip: max_tip.clone(),
            mode: GasFeeMode::Auto,
            quote: None,
            refreshing: false,
            disabled: false,
            on_event: Rc::new(on_event),
        }
    }

    #[must_use]
    pub const fn mode(mut self, mode: GasFeeMode) -> Self {
        self.mode = mode;
        self
    }

    #[must_use]
    pub fn quote(mut self, quote: Option<(String, String)>) -> Self {
        self.quote = quote;
        self
    }

    #[must_use]
    pub const fn refreshing(mut self, refreshing: bool) -> Self {
        self.refreshing = refreshing;
        self
    }

    fn child_id(&self, part: &str) -> SharedString {
        format!("{}-{part}", self.id).into()
    }

    fn field(
        &self,
        label: &'static str,
        target: GasFeeEditTarget,
        input: &Entity<InputState>,
        value: Option<&String>,
        cx: &App,
    ) -> gpui::Div {
        let content = if self.mode == GasFeeMode::Auto {
            let on_event = self.on_event.clone();
            div()
                .w_full()
                .input_h(Size::Medium)
                .px_3()
                .flex()
                .items_center()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().background)
                .text_size(APP_TEXT_SIZE)
                .line_height(relative(APP_TEXT_LINE_HEIGHT))
                .text_color(cx.theme().muted_foreground)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(value.cloned().unwrap_or_else(|| "unavailable".into())),
                )
                .child(
                    Button::new(self.child_id(match target {
                        GasFeeEditTarget::MaxFee => "edit-max-fee",
                        GasFeeEditTarget::MaxTip => "edit-max-tip",
                    }))
                    .icon(Icon::empty().path("ui/icons/pencil.svg"))
                    .ghost()
                    .xsmall()
                    .compact()
                    .accessibility_label(format!("Customize {label}"))
                    .tooltip("Customize gas fee")
                    .disabled(self.disabled || self.quote.is_none())
                    .on_click(move |_, window, cx| {
                        on_event(&GasFeeEditorEvent::Edit(target), window, cx);
                    }),
                )
                .into_any_element()
        } else {
            app_input(input)
                .px_3()
                .py_2()
                .aria_label(label)
                .disabled(self.disabled)
                .into_any_element()
        };
        div()
            .flex_1()
            .min_w_32()
            .flex()
            .flex_col()
            .gap_1()
            .child(app_muted_text(label))
            .child(content)
    }
}

impl Disableable for GasFeeEditor {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl RenderOnce for GasFeeEditor {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let auto = self.mode == GasFeeMode::Auto;
        let refreshing = auto && self.refreshing;
        let refresh_enabled = auto && !self.disabled && !self.refreshing;
        let on_refresh = self.on_event.clone();
        let refresh = app_button_base(self.child_id("refresh"))
            .ghost()
            .xsmall()
            .compact()
            .icon(Icon::empty().path(icons::refresh_ccw_icon_path()))
            .accessibility_label("Refresh gas price hint")
            .tooltip("Refresh gas price hint")
            .loading(refreshing)
            .disabled(!refresh_enabled)
            .opacity(if refresh_enabled || refreshing {
                1.0
            } else {
                0.45
            })
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                on_refresh(&GasFeeEditorEvent::Refresh, window, cx);
            });
        let on_auto = self.on_event.clone();
        let on_custom = self.on_event.clone();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(app_inline_control_row(
                "Gas fee",
                ButtonGroup::new(self.child_id("mode"))
                    .outline()
                    .compact()
                    .disabled(self.disabled)
                    // Child callbacks also run for keyboard activation in GPUI Component.
                    .child(
                        app_segment_button(
                            self.child_id("auto"),
                            "Auto",
                            auto,
                            self.disabled,
                            Some(refresh.into_any_element()),
                        )
                        .on_click(move |_, window, cx| {
                            on_auto(&GasFeeEditorEvent::Mode(GasFeeMode::Auto), window, cx);
                        }),
                    )
                    .child(
                        app_segment_button(
                            self.child_id("custom"),
                            "Custom",
                            !auto,
                            self.disabled,
                            None,
                        )
                        .on_click(move |_, window, cx| {
                            on_custom(&GasFeeEditorEvent::Mode(GasFeeMode::Custom), window, cx);
                        }),
                    ),
            ))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_end()
                    .gap_3()
                    .child(self.field(
                        "Max fee (gwei)",
                        GasFeeEditTarget::MaxFee,
                        &self.max_fee,
                        self.quote.as_ref().map(|quote| &quote.0),
                        cx,
                    ))
                    .child(self.field(
                        "Max tip (gwei)",
                        GasFeeEditTarget::MaxTip,
                        &self.max_tip,
                        self.quote.as_ref().map(|quote| &quote.1),
                        cx,
                    )),
            )
    }
}
