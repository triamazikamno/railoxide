#[allow(clippy::wildcard_imports)]
use super::*;

/// A rounded Sell or Buy panel, raised above the dialog. A danger border marks an
/// amount that doesn't fit one swap.
pub(super) fn amount_panel(danger: bool, cx: &App) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2p5()
        .rounded_lg()
        .border_1()
        .border_color(if danger {
            cx.theme().danger
        } else {
            cx.theme().border
        })
        .bg(cx.theme().group_box)
}

/// A form row with its label in the stealth account row's label column. In a narrow dialog
/// the control wraps under the label.
pub(super) fn labeled_row(label: &'static str, control: impl IntoElement) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(
            app_muted_text(label)
                .w(rems(ACCOUNT_LABEL_WIDTH))
                .flex_none(),
        )
        .child(
            div()
                .flex_1()
                .min_w(rems(ROW_CONTROL_MIN_WIDTH))
                .child(control),
        )
}

/// A panel's token select, at its trailing edge.
pub(super) fn token_pill(select: impl IntoElement) -> gpui::Div {
    div().w(rems(9.)).flex_none().child(select)
}

/// A started swap's own stealth account, which the form can't change.
pub(super) fn fixed_account(label: String) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .h_8()
        .px_3()
        .flex()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .child(
            app_muted_text(label)
                .min_w_0()
                .truncate()
                .font_family(theme::APP_MONO_FONT_FAMILY),
        )
}

/// A warning in the warning color. A `small` one is a secondary line, such as the one under the
/// stealth account select.
pub(super) fn warning_line(text: impl Into<SharedString>, small: bool) -> gpui::Div {
    let icon = Icon::new(IconName::TriangleAlert).text_color(rgb(theme::WARNING));
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(if small { icon.xsmall() } else { icon.small() })
        .child(
            app_text(text)
                .when(small, gpui::Styled::text_xs)
                .text_color(rgb(theme::WARNING))
                .whitespace_normal(),
        )
}

/// A panel's balance: secondary to the amount beside it, so muted and smaller.
pub(super) fn balance_text(text: impl Into<SharedString>) -> gpui::Div {
    app_muted_text(text).text_xs()
}

/// A wrapping danger Alert at `selector`, for a message with no action.
pub(super) fn error_alert(selector: &'static str, message: impl Into<SharedString>) -> gpui::Div {
    div()
        .debug_selector(move || selector.into())
        .w_full()
        .min_w_0()
        .child(Alert::error(selector, message.into()).small().min_w_0())
}

/// A wrapping danger Alert at `selector` with its `action` inside, such as Retry. Alert accepts
/// text only, so one frame contains its message and the button, as the high-cost warning's does.
pub(super) fn action_alert(
    selector: &'static str,
    message: String,
    action: Button,
    cx: &App,
) -> gpui::Div {
    alert_frame(selector, message, None, Some(action), None, cx)
}

/// The Details section of an [`alert_frame`]: the technical `text` behind the alert's message,
/// shown while `open`. Its toggle sets the form's state through `view`.
pub(super) struct AlertDetails {
    pub(super) text: String,
    pub(super) open: bool,
    pub(super) view: Entity<PrivateSwapsView>,
}

/// A wrapping danger Alert at `selector`. Under its message it has the provider's `status`
/// page to copy, then the collapsed `details`, when the failure has any. Its `action` comes
/// last, so it stays in view with the details open.
pub(super) fn alert_frame(
    selector: &'static str,
    message: String,
    details: Option<AlertDetails>,
    action: Option<Button>,
    status: Option<&'static str>,
    cx: &App,
) -> gpui::Div {
    let danger = cx.theme().danger;
    div()
        .debug_selector(move || selector.into())
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .px_3()
        .py_2()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(danger.mix_oklab(gpui::transparent_white(), 0.3))
        .bg(danger.mix_oklab(gpui::transparent_white(), 0.04))
        .child(
            div().min_w_0().child(
                Alert::error(selector, message)
                    .small()
                    .p_0()
                    .border_0()
                    .bg(gpui::transparent_black()),
            ),
        )
        .children(status.map(|url| {
            div()
                .debug_selector(move || format!("{selector}-status"))
                // Lines up with the message: the small Alert's icon is its text size, then 6px.
                .pl(rems(0.875))
                .ml(px(6.))
                .min_w_0()
                .flex()
                .flex_col()
                .child(app_muted_text("NEAR Intents incident status:").text_xs())
                .child(
                    div()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            app_muted_text(url.trim_start_matches("https://"))
                                .text_xs()
                                .min_w_0()
                                .truncate(),
                        )
                        .child(
                            clipboard_with_toast(
                                SharedString::from(format!("{selector}-status-copy")),
                                url,
                            )
                            .tooltip("Copy status page address"),
                        ),
                )
        }))
        .children(details.map(|details| alert_details(selector, details)))
        .children(action.map(|action| div().flex().child(action)))
}

/// The "Details" toggle of the alert at `selector` and, while open, the text with its copy
/// button. The selectors carry the alert's, as a form can show two alerts.
fn alert_details(selector: &'static str, details: AlertDetails) -> Collapsible {
    let AlertDetails { text, open, view } = details;
    let toggle = if open { "Hide details" } else { "Show details" };
    let copied = SharedString::from(text.clone());
    Collapsible::new()
        .open(open)
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .items_start()
        .gap_1()
        .child(
            app_button_base(SharedString::from(format!("{selector}-details-toggle")))
                .debug_selector(move || format!("{selector}-details-toggle"))
                .ghost()
                .small()
                .flex_none()
                .accessibility_label(toggle)
                .tooltip(toggle)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(app_button_label("Details"))
                        .child(
                            Icon::new(if open {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall()
                            .flex_none(),
                        ),
                )
                .on_click(move |_, _, cx| {
                    view.update(cx, |view, cx| view.set_quote_details_open(!open, cx));
                }),
        )
        .content(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .items_start()
                .gap_1()
                .child(
                    div()
                        .debug_selector(move || format!("{selector}-details"))
                        .w_full()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        // One element a line keeps the text's line breaks.
                        .children(text.lines().map(|line| {
                            app_muted_text(line.to_owned())
                                .text_xs()
                                .w_full()
                                .min_w_0()
                                .whitespace_normal()
                        })),
                )
                .child(
                    app_button(
                        SharedString::from(format!("{selector}-details-copy")),
                        "Copy",
                    )
                    .debug_selector(move || format!("{selector}-details-copy"))
                    .ghost()
                    .xsmall()
                    .flex_none()
                    .icon(IconName::Copy)
                    .on_click(move |_, window, cx| {
                        copy_to_clipboard_with_toast(copied.clone(), window, cx);
                    }),
                ),
        )
}

/// One details row. Indented rows are the costs taken from the output; `help` explains the
/// value in a tooltip.
pub(super) fn detail_row(
    label: impl Into<SharedString>,
    value: impl IntoElement,
    indented: bool,
    help: Option<String>,
) -> gpui::Div {
    let label = label.into();
    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .when(indented, gpui::Styled::pl_4)
        .child(
            div()
                .id(label.clone())
                .flex()
                .items_center()
                .gap_1()
                .child(app_muted_text(label))
                .when_some(help, |label, help| {
                    label
                        .child(
                            Icon::new(IconName::Info)
                                .xsmall()
                                .text_color(rgb(theme::TEXT_MUTED)),
                        )
                        .tooltip(move |window, cx| Tooltip::new(help.clone()).build(window, cx))
                }),
        )
        .child(div().flex_none().child(value))
}

/// Why the form can't use a receiver, under the Receiver field.
pub(super) const fn receiver_rejection_message(rejection: SwapReceiverRejection) -> &'static str {
    match rejection {
        // CoW reads a zero receiver as the order's owner, the stealth account.
        SwapReceiverRejection::ZeroAddress => {
            "The zero address would pay the stealth account making the swap. Enter the receiver's address."
        }
        SwapReceiverRejection::Executor => {
            "This is the stealth account making the swap. Choose Private balance to keep the tokens."
        }
        SwapReceiverRejection::Railgun => {
            "This is the Railgun contract. Tokens sent to it directly can't be recovered."
        }
        SwapReceiverRejection::Settlement
        | SwapReceiverRejection::VaultRelayer
        | SwapReceiverRejection::HooksTrampoline => {
            "This is a CoW Protocol contract. Tokens sent to it directly can't be recovered."
        }
    }
}

pub(super) const fn review_change_label(change: SwapReviewChange) -> &'static str {
    match change {
        SwapReviewChange::Delivery => "the delivery or receiver changed",
        SwapReviewChange::HookCost => "the network and hook limit needs review",
        SwapReviewChange::GasShare => "this approval predates gas shares and needs a full review",
        SwapReviewChange::GasAllowance { .. } => "the gas you allow increased",
        SwapReviewChange::Validity { .. } => "the order validity changed",
        SwapReviewChange::ShieldFee { .. } => "the Railgun shield fee changed",
        SwapReviewChange::DestinationShieldFee { .. } => {
            "the Railgun shield fee on the destination network changed"
        }
        SwapReviewChange::UnshieldFee { .. } => "the Railgun unshield fee changed",
        SwapReviewChange::QuoteDeviates => "the quote moved away from the anchor price",
        SwapReviewChange::PriceVerification => "the price check changed",
        SwapReviewChange::PriceUnavailable => "the independent price became unavailable",
        SwapReviewChange::Minimum { .. } => "the quote fell below the minimum you approved then",
        SwapReviewChange::DestinationMinimum { .. } => {
            "the bridge quote fell below the minimum you approved then"
        }
        SwapReviewChange::DeliveryAllowance { .. } => {
            "shielding on the destination network got more expensive, which left less than the minimum you approved then"
        }
        SwapReviewChange::Accounts => "the stealth accounts or their setup changed",
        SwapReviewChange::ApprovalPlan => "the token approval the swap needs changed",
        SwapReviewChange::ContinuingApproval { .. } => {
            "an earlier order of this swap can still deliver less on the destination network"
        }
        SwapReviewChange::SpeedUpBuyAmount { .. } => {
            "this doesn't lower the open order's minimum, so it wouldn't speed the swap up"
        }
    }
}

/// What a setup submission leaves for the user to know, from each setup's own problem. A
/// private Bridge swap's two setups are named by their `networks`: the swap's own, then the
/// destination's.
pub(super) fn setup_problems(
    origin: Option<String>,
    destination: Option<String>,
    networks: Option<&(String, String)>,
) -> Option<String> {
    let Some((origin_network, destination_network)) = networks else {
        return origin;
    };
    let problems = [(origin_network, origin), (destination_network, destination)]
        .into_iter()
        .filter_map(|(network, problem)| Some(format!("{network}: {}", problem?)))
        .collect::<Vec<_>>();
    (!problems.is_empty()).then(|| problems.join(" "))
}

/// What a broadcaster's answer leaves for the user to know. `None` when it accepted.
pub(in crate::root::private_swap) fn broadcaster_result_problem(
    result: &PublicBroadcasterResultKind,
    what: &str,
) -> Option<String> {
    match result {
        PublicBroadcasterResultKind::Submitted { .. } => None,
        PublicBroadcasterResultKind::Failed { error } => Some(format!(
            "The broadcaster reported a problem with the {what}: {error}"
        )),
        PublicBroadcasterResultKind::TimedOut => Some(format!(
            "No broadcaster response yet. The {what} stays tracked, and the swap updates when it lands."
        )),
    }
}
