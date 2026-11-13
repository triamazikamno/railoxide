use std::sync::Arc;

use gpui::{
    Anchor, AnyElement, App, AppContext, ClickEvent, Context, Entity, FontWeight,
    InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, StyledImage as _, Window, div, img,
    prelude::FluentBuilder as _, px, relative, rems, rgb,
};
use gpui_component::{
    ActiveTheme, Icon, IconName, IndexPath, Selectable, Sizable,
    alert::Alert,
    button::ButtonVariants,
    collapsible::Collapsible,
    description_list::{DescriptionItem, DescriptionList},
    popover::Popover,
    select::{SearchableVec, Select, SelectState},
    tooltip::Tooltip,
};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_amount_text, app_button_base, app_muted_text, app_strong_text, app_text};
use ui::hint::hint_card;
use ui::theme::{self, APP_MONO_FONT_FAMILY};

use crate::root::ui_helpers::{network_mention, network_token_icon};

use super::summary::{
    SpendAuthorizationAmountDelta, SpendAuthorizationCard, SpendAuthorizationCardLine,
    SpendAuthorizationDetails, SpendAuthorizationDetailsContent, SpendAuthorizationDisclosure,
    SpendAuthorizationHint, SpendAuthorizationLabel, SpendAuthorizationPayload,
    SpendAuthorizationSteps, SpendAuthorizationSummary, SpendAuthorizationSummaryRow,
};
use super::{SpendAuthorizationLifetime, SpendAuthorizationLifetimeSelect};

const SUMMARY_RECIPIENT_PREFIX_CHARS: usize = 8;
const SUMMARY_RECIPIENT_SUFFIX_CHARS: usize = 8;
const SUMMARY_RECIPIENT_SHORTEN_THRESHOLD_CHARS: usize = 28;
/// Wide enough for the longest lifetime label, "Until vault locks/app closes".
const SPEND_AUTHORIZATION_LIFETIME_SELECT_WIDTH: gpui::Pixels = px(248.0);
/// The least height of a compact row, which fits its info button.
const SUMMARY_COMPACT_ROW_MIN_HEIGHT: gpui::Pixels = px(26.0);

pub(super) struct SpendAuthorizationPayloadDisclosure {
    payload: SpendAuthorizationPayload,
    open: bool,
}

impl SpendAuthorizationPayloadDisclosure {
    pub(super) const fn new(payload: SpendAuthorizationPayload) -> Self {
        Self {
            payload,
            open: false,
        }
    }

    fn toggle(&mut self, cx: &mut Context<'_, Self>) {
        self.open = !self.open;
        cx.notify();
    }
}

impl gpui::Render for SpendAuthorizationPayloadDisclosure {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let disclosure = cx.entity();
        render_spend_authorization_payload(&self.payload, self.open, move |_, _, cx| {
            disclosure.update(cx, Self::toggle);
        })
    }
}

/// A details disclosure for a dialog without its own content entity.
pub(super) struct SpendAuthorizationDetailsDisclosure {
    id: &'static str,
    details: SpendAuthorizationDetails,
    open: bool,
}

impl SpendAuthorizationDetailsDisclosure {
    pub(super) const fn new(id: &'static str, details: SpendAuthorizationDetails) -> Self {
        Self {
            id,
            details,
            open: false,
        }
    }

    fn toggle(&mut self, cx: &mut Context<'_, Self>) {
        self.open = !self.open;
        cx.notify();
    }
}

impl gpui::Render for SpendAuthorizationDetailsDisclosure {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let disclosure = cx.entity();
        render_spend_authorization_details(
            self.id,
            &self.details,
            self.open,
            move |_, _, cx| {
                disclosure.update(cx, Self::toggle);
            },
            cx,
        )
    }
}

/// The summary's cards, row group, rows, details disclosure and disclosure alert. The row
/// group, compact rows and the details disclosure form one list.
pub(super) fn render_spend_authorization_summary(
    summary: &SpendAuthorizationSummary,
    row_group: Option<AnyElement>,
    details: Option<AnyElement>,
    cx: &App,
) -> gpui::Div {
    // The rows count on from the row group's, so each row's controls keep their own ids.
    let first_row = summary.grouped_rows().len();
    let rows =
        if summary.compact_rows {
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .children(summary.rows.iter().enumerate().map(|(row_index, row)| {
                    render_spend_authorization_compact_row(first_row + row_index, row, cx)
                }))
                .into_any_element()
        } else {
            DescriptionList::vertical()
                .large()
                .bordered(false)
                .columns(1)
                .children(
                    summary.rows.iter().enumerate().map(|(row_index, row)| {
                        spend_authorization_summary_item(row_index, row, cx)
                    }),
                )
                .into_any_element()
        };
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_3()
        .when_some(summary.cards.as_ref(), |this, cards| {
            this.child(div().w_full().min_w_0().flex().flex_col().gap_1().children(
                cards.iter().enumerate().map(|(card_index, card)| {
                    render_spend_authorization_card(card_index, card, cx)
                }),
            ))
        })
        .map(|this| {
            if row_group.is_none() && details.is_none() {
                return this.child(rows);
            }
            this.child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .children(row_group)
                    .child(rows)
                    .children(details),
            )
        })
        .when_some(summary.disclosure.as_ref(), |this, disclosure| {
            this.child(render_spend_authorization_disclosure(disclosure))
        })
}

/// A change's text at the small size: in the danger colour when adverse, else the success
/// colour.
fn spend_authorization_delta_text(delta: &SpendAuthorizationAmountDelta, cx: &App) -> gpui::Div {
    app_text(delta.text.clone())
        .flex_none()
        .text_xs()
        .whitespace_nowrap()
        .text_color(if delta.adverse {
            cx.theme().danger
        } else {
            cx.theme().success
        })
}

/// A card's label, its icon and amount with the amount's change beside it and the USD value at
/// the right edge, then its lines.
fn render_spend_authorization_card(
    card_index: usize,
    card: &SpendAuthorizationCard,
    cx: &App,
) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE))
        .child(spend_authorization_label(
            &card.label,
            rems(0.75).into(),
            |text| app_muted_text(text).text_xs(),
        ))
        .child(render_spend_authorization_card_amount_row(card, cx))
        .children(
            card.lines
                .iter()
                .map(|line| render_spend_authorization_card_line(card_index, line)),
        )
}

/// A card's icon and amount with the amount's change beside it and the USD value at the right
/// edge. In a narrow dialog the USD value wraps under the amount, and the change under the
/// amount before that. The amount itself never breaks.
fn render_spend_authorization_card_amount_row(
    card: &SpendAuthorizationCard,
    cx: &App,
) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x_2()
        .child(
            div()
                .flex_auto()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .children(match card.network {
                    // Larger than the plain icon, so the network chip stays readable
                    // beside the card's amount.
                    Some(chain_id) => Some(
                        network_token_icon(card.icon.clone(), chain_id, 1.75).into_any_element(),
                    ),
                    None => card.icon.clone().map(|icon| {
                        img(icon)
                            .size(px(20.0))
                            .rounded_full()
                            .flex_none()
                            .into_any_element()
                    }),
                })
                .child(
                    div()
                        .flex_auto()
                        .min_w_0()
                        .flex()
                        .flex_wrap()
                        .items_baseline()
                        .gap_x_2()
                        .child(
                            app_amount_text(card.amount.to_string())
                                .text_color(rgb(theme::TEXT))
                                .whitespace_nowrap(),
                        )
                        .children(
                            card.delta
                                .as_ref()
                                .map(|delta| spend_authorization_delta_text(delta, cx)),
                        ),
                ),
        )
        .children(card.usd.as_ref().map(|usd| {
            app_muted_text(usd.to_string())
                .flex_none()
                .ml_auto()
                .whitespace_nowrap()
        }))
}

/// One of a card's lines: its text, or its receiver's or account's address line.
fn render_spend_authorization_card_line(
    card_index: usize,
    line: &SpendAuthorizationCardLine,
) -> gpui::Div {
    match line {
        SpendAuthorizationCardLine::Text {
            before,
            strong,
            after,
        } => div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_wrap()
            .items_baseline()
            .gap_x_1()
            .when(!before.is_empty(), |this| {
                this.child(app_muted_text(before.to_string()).text_xs())
            })
            .when(!strong.is_empty(), |this| {
                this.child(app_strong_text(strong.to_string()).text_xs())
            })
            .when(!after.is_empty(), |this| {
                this.child(app_muted_text(after.to_string()).text_xs())
            }),
        SpendAuthorizationCardLine::Receiver {
            address,
            short,
            label,
        } => spend_authorization_card_address_line(
            card_index,
            "to",
            None,
            address,
            short,
            label.clone(),
            "Copy receiver",
        ),
        SpendAuthorizationCardLine::Account { name, address } => {
            spend_authorization_card_address_line(
                card_index,
                "from",
                Some(name),
                address,
                &spend_authorization_recipient_display(address),
                None,
                "Copy address",
            )
        }
    }
}

/// A card's address line: `lead`, the account's `name` when the line shows it, and `address`
/// in its `short` form with a copy button. Hovering the address shows it in full, after
/// `hover_label` when it has one.
fn spend_authorization_card_address_line(
    card_index: usize,
    lead: &'static str,
    name: Option<&Arc<str>>,
    address: &Arc<str>,
    short: &str,
    hover_label: Option<Arc<str>>,
    copy_tooltip: &'static str,
) -> gpui::Div {
    let full_address = address.to_string();
    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .gap_1()
        .child(app_muted_text(lead).text_xs())
        .children(name.map(|name| {
            app_text(name.to_string())
                .text_xs()
                .text_color(rgb(theme::TEXT))
        }))
        .child(
            div()
                .id(("wallet-spend-auth-card-receiver", card_index))
                .min_w_0()
                .tooltip(move |window, cx| {
                    let (full_address, label) = (full_address.clone(), hover_label.clone());
                    Tooltip::element(move |_, _| {
                        div()
                            .flex()
                            .flex_col()
                            .children(label.as_ref().map(ToString::to_string))
                            .child(
                                div()
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .child(full_address.clone()),
                            )
                    })
                    .build(window, cx)
                })
                .child(
                    app_text(short.to_owned())
                        .text_xs()
                        .text_color(rgb(theme::TEXT))
                        .font_family(APP_MONO_FONT_FAMILY),
                ),
        )
        .child(
            div()
                .id(("wallet-spend-auth-card-copy-action", card_index))
                .flex_none()
                .tooltip(move |window, cx| Tooltip::new(copy_tooltip).build(window, cx))
                .child(clipboard_with_toast(
                    ("wallet-spend-auth-card-copy", card_index),
                    address.to_string(),
                )),
        )
}

/// A row on one line: its label, its value at the right, the value's change, and a slot for
/// its info button. The slot stays empty without a hint, so the values line up. A value too wide
/// to sit beside its label wraps under it, with its change and info button.
fn render_spend_authorization_compact_row(
    row_index: usize,
    row: &SpendAuthorizationSummaryRow,
    cx: &App,
) -> gpui::Div {
    let value = render_spend_authorization_compact_row_value(row_index, row);
    let label = match &row.label_network {
        Some(label) => spend_authorization_row_label(label, app_muted_text),
        None => app_muted_text(row.label.to_string()),
    };
    div()
        .w_full()
        .min_w_0()
        .min_h(SUMMARY_COMPACT_ROW_MIN_HEIGHT)
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x_2()
        .child(label.flex_none().whitespace_nowrap())
        .child(value)
        .children(
            row.delta
                .as_ref()
                .map(|delta| spend_authorization_delta_text(delta, cx)),
        )
        .child(
            div()
                .flex_none()
                .size_5()
                .flex()
                .items_center()
                .justify_center()
                .children(row.hint.as_ref().map(|hint| {
                    spend_authorization_hint(
                        format!("wallet-spend-auth-row-hint-{row_index}"),
                        hint,
                        None,
                    )
                })),
        )
}

/// A compact row's value: shortened with a copy button, or its text after a network or the
/// row's mark, at the right edge.
fn render_spend_authorization_compact_row_value(
    row_index: usize,
    row: &SpendAuthorizationSummaryRow,
) -> gpui::Div {
    if row.shortened_copyable {
        let copy_tooltip = row.copy_tooltip();
        div()
            .flex_auto()
            .min_w_0()
            .flex()
            .items_center()
            .justify_end()
            .gap_1()
            .child(
                app_text(row.shortened_value())
                    .min_w_0()
                    .text_color(rgb(theme::TEXT))
                    .font_family(APP_MONO_FONT_FAMILY),
            )
            .child(
                div()
                    .id(("wallet-spend-auth-copy-action", row_index))
                    .flex_none()
                    .tooltip(move |window, cx| Tooltip::new(copy_tooltip.clone()).build(window, cx))
                    .child(clipboard_with_toast(
                        ("wallet-spend-auth-copy", row_index),
                        row.value.to_string(),
                    )),
            )
    } else {
        let value = match (&row.value_network, &row.icon_path) {
            // The network wraps under the text before it, at the right edge too.
            (Some(value), _) => spend_authorization_row_label(value, app_text)
                .flex_wrap()
                .justify_end(),
            // The row's mark before its text, the size of a network's in a label.
            (None, Some(icon)) => div()
                .flex()
                .items_center()
                .justify_end()
                .gap_x_1()
                .child(
                    img(icon.clone())
                        .size(rems(0.75))
                        .flex_none()
                        .object_fit(gpui::ObjectFit::Contain),
                )
                .child(app_text(row.value.to_string()).min_w_0()),
            (None, None) => app_text(row.value.to_string()),
        };
        value
            .flex_auto()
            .min_w_0()
            .text_right()
            .text_color(rgb(theme::TEXT))
            .whitespace_normal()
    }
}

/// The disclosure alert: "Made public:" and the summary on one line, and an info button
/// inside its right edge. The whole alert shows the hint's card on hover and opens it on click.
/// A hint with a warning makes it a warning alert.
fn render_spend_authorization_disclosure(disclosure: &SpendAuthorizationDisclosure) -> gpui::Div {
    let id = "wallet-spend-auth-disclosure-alert";
    let summary = format!("Made public: {}", disclosure.summary);
    let alert = if disclosure.hint.warning.is_some() {
        Alert::warning(id, summary)
    } else {
        Alert::info(id, summary)
    };
    div().w_full().min_w_0().child(spend_authorization_hint(
        "wallet-spend-auth-disclosure",
        &disclosure.hint,
        Some(
            alert
                .small()
                // Room for the info button.
                .pr_10()
                .into_any_element(),
        ),
    ))
}

/// An info button for `hint`: the hint's card as a tooltip on hover, and in a popover on click
/// or from the keyboard. Escape closes the popover and returns focus to the button. With
/// `line`, the button sits inside that element's right edge, and the whole element shows and
/// opens the card.
fn spend_authorization_hint(
    id: impl Into<SharedString>,
    hint: &SpendAuthorizationHint,
    line: Option<AnyElement>,
) -> Popover {
    let id = id.into();
    let content = hint.clone();
    Popover::new(id.clone())
        .anchor(Anchor::TopRight)
        .trigger(SpendAuthorizationHintTrigger {
            id,
            hint: hint.clone(),
            line,
            open: false,
        })
        .content(move |_, window, _| spend_authorization_hint_card(&content, window))
}

/// A hint popover's trigger. The popover marks it selected while open, which drops the hover
/// tooltip so the card doesn't show twice.
#[derive(IntoElement)]
struct SpendAuthorizationHintTrigger {
    id: SharedString,
    hint: SpendAuthorizationHint,
    line: Option<AnyElement>,
    open: bool,
}

impl Selectable for SpendAuthorizationHintTrigger {
    fn selected(mut self, selected: bool) -> Self {
        self.open = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.open
    }
}

impl RenderOnce for SpendAuthorizationHintTrigger {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            hint,
            line,
            open,
        } = self;
        let button = app_button_base(SharedString::from(format!("{id}-button")))
            .ghost()
            .xsmall()
            .icon(IconName::Info)
            .selected(open)
            .accessibility_label(hint.title.to_string());
        let selector = id.to_string();
        div()
            .id(SharedString::from(format!("{id}-hover")))
            .debug_selector(move || selector)
            .map(|this| match line {
                Some(line) => this
                    .relative()
                    .w_full()
                    .min_w_0()
                    .cursor_pointer()
                    .child(line)
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right_2()
                            .flex()
                            .items_center()
                            .child(button),
                    ),
                None => this.flex_none().child(button),
            })
            .when(!open, |this| {
                this.tooltip(move |window, cx| {
                    let hint = hint.clone();
                    Tooltip::element(move |window, _| spend_authorization_hint_card(&hint, window))
                        .build(window, cx)
                })
            })
    }
}

/// `hint` as a hint card: its paragraphs under the title, then its warning in the warning
/// colour, which the title takes too, then its fact under a rule.
fn spend_authorization_hint_card(hint: &SpendAuthorizationHint, window: &Window) -> gpui::Div {
    let title_color = if hint.warning.is_some() {
        theme::WARNING
    } else {
        theme::INFO
    };
    hint_card(hint.title.to_string(), title_color, window)
        .children(
            hint.paragraphs
                .iter()
                .map(|paragraph| div().whitespace_normal().child(paragraph.to_string())),
        )
        .children(hint.warning.as_ref().map(|warning| {
            div()
                .whitespace_normal()
                .text_color(rgb(theme::WARNING))
                .child(warning.to_string())
        }))
        .children(hint.fact.as_ref().map(|(label, value)| {
            div()
                .w_full()
                .pt_1()
                .border_t_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .flex()
                .items_baseline()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(label.to_string()),
                )
                .child(div().flex_none().child(value.to_string()))
        }))
}

/// `label` as the text `styled` makes of a string. The mark of a network it names is `mark`,
/// that text's size, square.
fn spend_authorization_label(
    label: &SpendAuthorizationLabel,
    mark: gpui::AbsoluteLength,
    styled: impl FnOnce(String) -> gpui::Div,
) -> gpui::Div {
    let text = styled(label.prefix.to_string());
    let Some(chain_id) = label.network else {
        return text;
    };
    text.flex().items_center().gap_x_1().child(
        network_mention(chain_id, mark).when(!label.suffix.is_empty(), |mention| {
            mention.child(label.suffix.to_string())
        }),
    )
}

/// A compact row's label or value that names a network, as the text `styled` makes of a
/// string. The network's mark is the size of the one in a card's label.
fn spend_authorization_row_label(
    label: &SpendAuthorizationLabel,
    styled: impl FnOnce(String) -> gpui::Div,
) -> gpui::Div {
    spend_authorization_label(label, rems(0.75).into(), styled)
}

/// A dialog title, with a chip naming the summary's network beside it when it has one.
pub(super) fn spend_authorization_title(title: &str, network: Option<u64>) -> gpui::Div {
    let title = app_strong_text(title.to_owned());
    let Some(chain_id) = network else {
        return title;
    };
    div()
        .min_w_0()
        // Clear of the dialog's close button, so the chip wraps under the title instead.
        .pr_6()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(title)
        .child(
            network_mention(chain_id, rems(0.75).into())
                .flex_none()
                .text_xs()
                .line_height(relative(theme::APP_TEXT_LINE_HEIGHT))
                .text_color(rgb(theme::TEXT_MUTED))
                .px_2()
                .rounded_full()
                .border_1()
                .border_color(rgb(theme::BORDER)),
        )
}

/// A stepper: numbered badges and their labels joined by a connector, with an info button at
/// the end. A completed step shows a check in the success colour, the current one a filled
/// badge, and a later one an outlined badge and a muted label.
/// A step that doesn't fit beside the one before it wraps to the next line, and a label wider
/// than the strip truncates.
pub(in crate::root) fn render_spend_authorization_steps(
    steps: &SpendAuthorizationSteps,
) -> gpui::Div {
    let mut strip = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .py_2()
        .pl_3()
        .pr_2()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE_HOVER_SUBTLE));
    for (index, label) in steps.labels.iter().enumerate() {
        let number = index + 1;
        let (done, current) = (number < steps.current, number == steps.current);
        if index > 0 {
            // The connector out of a completed step.
            strip = strip.child(div().flex_1().min_w(px(16.0)).h(px(1.0)).bg(rgb(
                if index < steps.current {
                    theme::SUCCESS
                } else {
                    theme::BORDER_STRONG
                },
            )));
        }
        let badge = render_spend_authorization_step_badge(number, done, current);
        let label_color = if done {
            theme::SUCCESS
        } else if current {
            theme::TEXT
        } else {
            theme::TEXT_MUTED
        };
        strip = strip.child(
            div()
                .min_w_0()
                .max_w_full()
                .flex()
                .items_center()
                .gap_2()
                .child(badge)
                .child(
                    spend_authorization_label(label, theme::APP_TEXT_SIZE.into(), app_text)
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(label_color)),
                ),
        );
    }
    strip.child(spend_authorization_hint(
        "wallet-spend-auth-steps-hint",
        &steps.hint,
        None,
    ))
}

/// A step's numbered badge: a check in the success colour when `done`, filled when `current`,
/// else outlined.
fn render_spend_authorization_step_badge(number: usize, done: bool, current: bool) -> gpui::Div {
    let badge = div()
        .flex_none()
        .size(px(22.0))
        .rounded_full()
        .border_1()
        .flex()
        .items_center()
        .justify_center();
    if done {
        badge
            .border_color(rgb(theme::SUCCESS))
            .bg(rgb(theme::SUCCESS))
            .child(
                Icon::new(IconName::Check)
                    .xsmall()
                    .text_color(rgb(theme::PRIMARY_FOREGROUND)),
            )
    } else if current {
        badge
            .border_color(rgb(theme::PRIMARY))
            .bg(rgb(theme::PRIMARY))
            .child(
                app_text(number.to_string())
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(theme::PRIMARY_FOREGROUND)),
            )
    } else {
        badge.border_color(rgb(theme::BORDER_STRONG)).child(
            app_muted_text(number.to_string())
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD),
        )
    }
}

/// A details disclosure as one more compact row: its title, its summary at the right, and a
/// chevron where the rows have their info button. Open, its lines and note or its rows follow,
/// indented, and a row group's summary is left out, since its rows carry the amounts. `id`
/// names its toggle.
pub(super) fn render_spend_authorization_details(
    id: &'static str,
    details: &SpendAuthorizationDetails,
    open: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Collapsible {
    let content = render_spend_authorization_details_content(details, cx);
    let header = render_spend_authorization_details_header(id, details, open, on_toggle);
    Collapsible::new()
        .open(open)
        .w_full()
        .child(header)
        .content(content)
}

/// A details disclosure's open content: its lines and note, or its rows, indented.
fn render_spend_authorization_details_content(
    details: &SpendAuthorizationDetails,
    cx: &App,
) -> gpui::Div {
    let list = div()
        .min_w_0()
        .ml_1()
        .pl_3()
        .flex()
        .flex_col()
        .border_l_1()
        .border_color(rgb(theme::BORDER_SUBTLE));
    let content = div().w_full().min_w_0().flex().flex_col().gap_1().py_1();
    match &details.content {
        SpendAuthorizationDetailsContent::Lines { rows, note } => content
            // The rows end where the values above end, clear of the chevron's column.
            .pr(px(28.0))
            .child(list.gap_1().children(rows.iter().map(|(label, value)| {
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .items_start()
                    .justify_between()
                    .gap_x_3()
                    .child(
                        app_muted_text(label.to_string())
                            .flex_none()
                            .whitespace_nowrap(),
                    )
                    .child(
                        app_text(value.to_string())
                            .flex_auto()
                            .min_w_0()
                            .text_right()
                            .text_color(rgb(theme::TEXT))
                            .whitespace_normal(),
                    )
            })))
            .when_some(note.as_ref(), |this, note| {
                this.child(
                    app_muted_text(note.to_string())
                        .pl_4()
                        .text_xs()
                        .whitespace_normal(),
                )
            }),
        // Each row's info button sits in the chevron's column.
        SpendAuthorizationDetailsContent::Rows(rows) => content.child(
            list.children(rows.iter().enumerate().map(|(row_index, row)| {
                render_spend_authorization_compact_row(row_index, row, cx)
            })),
        ),
    }
}

/// A details disclosure's toggle row: its title, its summary at the right unless open on a row
/// group, and a chevron. `id` names the toggle.
fn render_spend_authorization_details_header(
    id: &'static str,
    details: &SpendAuthorizationDetails,
    open: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let summarized = !open || !matches!(details.content, SpendAuthorizationDetailsContent::Rows(_));
    div()
        .id(id)
        .w_full()
        .min_w_0()
        .min_h(SUMMARY_COMPACT_ROW_MIN_HEIGHT)
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x_2()
        .cursor_pointer()
        .on_click(on_toggle)
        .child(
            app_muted_text(details.title.to_string())
                .flex_none()
                .whitespace_nowrap(),
        )
        // The summary and its chevron wrap under the title together.
        .child(
            div()
                .flex_auto()
                .min_w_0()
                .flex()
                .items_center()
                .justify_end()
                .gap_x_2()
                .when(summarized, |this| {
                    this.child(
                        app_text(details.collapsed_summary.to_string())
                            .min_w_0()
                            .text_right()
                            .text_color(rgb(theme::TEXT))
                            .whitespace_normal(),
                    )
                })
                .child(
                    div()
                        .flex_none()
                        .size_5()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            Icon::new(if open {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall(),
                        ),
                ),
        )
}

pub(super) fn render_spend_authorization_payload(
    payload: &SpendAuthorizationPayload,
    open: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Collapsible {
    let label = payload.label.to_string();
    let value = payload.value.to_string();
    Collapsible::new()
        .open(open)
        .w_full()
        .child(
            div()
                .id("wallet-spend-auth-payload-toggle")
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .cursor_pointer()
                .on_click(on_toggle)
                .child(app_strong_text(if open {
                    format!("Hide full {label}")
                } else {
                    format!("Show full {label}")
                }))
                .child(
                    gpui_component::Icon::new(if open {
                        gpui_component::IconName::ChevronUp
                    } else {
                        gpui_component::IconName::ChevronDown
                    })
                    .xsmall(),
                ),
        )
        .content(
            div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .items_start()
                .gap_2()
                .pt(px(6.0))
                .child(
                    app_text(value.clone())
                        .flex_1()
                        .min_w(px(0.0))
                        .font_family(APP_MONO_FONT_FAMILY)
                        .text_size(px(12.0))
                        .whitespace_normal(),
                )
                .child(clipboard_with_toast(
                    "wallet-spend-auth-payload-copy",
                    value,
                )),
        )
}

fn spend_authorization_summary_item(
    row_index: usize,
    row: &SpendAuthorizationSummaryRow,
    cx: &App,
) -> DescriptionItem {
    DescriptionItem::new(row.label.to_string())
        .value(spend_authorization_summary_value(row_index, row, cx))
}

fn spend_authorization_summary_value(
    row_index: usize,
    row: &SpendAuthorizationSummaryRow,
    cx: &App,
) -> AnyElement {
    if let Some(icon_path) = row.icon_path.clone() {
        return div()
            .w_full()
            .min_w(px(0.0))
            .flex()
            .items_center()
            .gap_1()
            .py(px(2.0))
            .text_color(rgb(theme::TEXT))
            .child(img(icon_path).size(px(20.0)).rounded_full().flex_none())
            .child(
                app_text(row.value.to_string())
                    .flex_1()
                    .min_w(px(0.0))
                    .whitespace_normal(),
            )
            .into_any_element();
    }

    if row.shortened_copyable {
        return spend_authorization_summary_copyable_value(row_index, row).into_any_element();
    }

    if let Some(delta) = &row.delta {
        return div()
            .w_full()
            .min_w(px(0.0))
            .flex()
            .flex_wrap()
            .items_baseline()
            .gap_x_2()
            .py(px(2.0))
            .child(
                app_text(row.value.to_string())
                    .min_w(px(0.0))
                    .whitespace_normal()
                    .text_color(rgb(theme::TEXT)),
            )
            .child(
                app_muted_text(delta.text.clone())
                    .min_w(px(0.0))
                    .whitespace_normal()
                    .when(delta.adverse, |this| this.text_color(cx.theme().danger)),
            )
            .into_any_element();
    }

    app_text(row.value.to_string())
        .w_full()
        .min_w(px(0.0))
        .py(px(2.0))
        .text_color(rgb(theme::TEXT))
        .whitespace_normal()
        .into_any_element()
}

/// A summary value shortened, with a copy button for the full value.
fn spend_authorization_summary_copyable_value(
    row_index: usize,
    row: &SpendAuthorizationSummaryRow,
) -> gpui::Div {
    let display_value = row.shortened_value();
    let copy_tooltip = row.copy_tooltip();
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_2()
        .py(px(2.0))
        .child(
            app_text(display_value)
                .min_w(px(0.0))
                .line_height(px(17.0))
                .text_color(rgb(theme::TEXT))
                .font_family(APP_MONO_FONT_FAMILY)
                .whitespace_normal(),
        )
        .child(
            div()
                .id(("wallet-spend-auth-copy-action", row_index))
                .flex_none()
                .tooltip(move |window, cx| Tooltip::new(copy_tooltip.clone()).build(window, cx))
                .child(clipboard_with_toast(
                    ("wallet-spend-auth-copy", row_index),
                    row.value.to_string(),
                )),
        )
}

pub(in crate::root) fn spend_authorization_recipient_display(value: &str) -> String {
    if value.chars().count() <= SUMMARY_RECIPIENT_SHORTEN_THRESHOLD_CHARS {
        return value.to_string();
    }
    let prefix: String = value.chars().take(SUMMARY_RECIPIENT_PREFIX_CHARS).collect();
    let suffix_chars: Vec<char> = value
        .chars()
        .rev()
        .take(SUMMARY_RECIPIENT_SUFFIX_CHARS)
        .collect();
    let suffix: String = suffix_chars.into_iter().rev().collect();
    format!("{prefix}...{suffix}")
}

pub(super) fn new_spend_authorization_lifetime_select<T: 'static>(
    initial: SpendAuthorizationLifetime,
    window: &mut Window,
    cx: &mut Context<'_, T>,
) -> Entity<SpendAuthorizationLifetimeSelect> {
    let selected = SpendAuthorizationLifetime::ALL
        .iter()
        .position(|lifetime| *lifetime == initial)
        .map(IndexPath::new);
    cx.new(|cx| {
        SelectState::new(
            SearchableVec::new(SpendAuthorizationLifetime::ALL),
            selected,
            window,
            cx,
        )
    })
}

/// "Remember authorization" and its select on one row. The select drops under the label when
/// the row is too narrow for both.
pub(super) fn render_spend_authorization_lifetime_row(
    select: &Entity<SpendAuthorizationLifetimeSelect>,
    disabled: bool,
) -> gpui::Div {
    let control = div()
        .flex_none()
        .w(SPEND_AUTHORIZATION_LIFETIME_SELECT_WIDTH)
        .max_w_full()
        .child(
            Select::new(select)
                .small()
                .w_full()
                .menu_width(SPEND_AUTHORIZATION_LIFETIME_SELECT_WIDTH)
                .disabled(disabled),
        );
    #[cfg(test)]
    let control = control.debug_selector(|| "wallet-spend-auth-lifetime-select".to_owned());
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_x_3()
        .gap_y_1()
        .child(app_muted_text("Remember authorization").whitespace_nowrap())
        .child(control)
}
