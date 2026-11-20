#[allow(clippy::wildcard_imports)]
use super::*;

/// What the setup broadcaster controls show of one route, captured when their holder renders.
pub(super) struct SetupRouteSettings {
    fee_options: Vec<PublicBroadcasterFeeTokenOption>,
    fee_token: Address,
    allow_out_of_range: bool,
    favorites_only: bool,
    random_selected: bool,
    specific_label: String,
    candidate_count: usize,
    busy: bool,
}

impl SetupRouteSettings {
    pub(super) fn of(route: &SetupRoute, busy: bool) -> Self {
        Self {
            fee_options: route.fee_options.clone(),
            fee_token: route.fee_token.unwrap_or_default(),
            allow_out_of_range: route.allow_out_of_range,
            favorites_only: route.favorites_only,
            random_selected: route.selected.is_none(),
            specific_label: selected_broadcaster_label(&route.choice(), &route.candidates),
            candidate_count: route.candidates.len(),
            busy,
        }
    }
}

/// The shared broadcaster settings with the fee token selector, driving `side`'s setup route
/// of the open form or retry. Specific broadcaster opens the broadcaster picker on top of the
/// swap dialog.
pub(super) fn setup_route_settings(
    view: Entity<PrivateSwapsView>,
    side: SetupSide,
    route: &SetupRouteSettings,
) -> gpui::Stateful<gpui::Div> {
    use ui::private_action::BroadcasterSettingsEvent as Event;
    let (token_id, settings_id) = match side {
        SetupSide::Origin => ("swap-setup-fee-token", "swap-setup-broadcaster-settings"),
        SetupSide::Destination => (
            "swap-destination-setup-fee-token",
            "swap-destination-setup-broadcaster-settings",
        ),
    };
    let fee_view = view.clone();
    let fee_token = fee_token_selector(
        token_id.into(),
        &route.fee_options,
        route.fee_token,
        route.busy,
        move |token, _, cx| {
            fee_view.update(cx, |view, cx| {
                if let Some(route) = view.active_setup_route_mut(side) {
                    route.fee_token = Some(token);
                    route.fee_token_chosen = true;
                    route.invalidate_estimate();
                }
                view.refresh_setup_route(cx);
            });
        },
    );
    ui::private_action::broadcaster_settings_fields(
        settings_id,
        ui::private_action::BroadcasterSettings {
            allow_out_of_range: route.allow_out_of_range,
            favorites_only: route.favorites_only,
            random_selected: route.random_selected,
            specific_label: route.specific_label.clone(),
            candidate_count: route.candidate_count,
            disabled: route.busy,
        },
        fee_token,
        None,
        move |event, window, cx| {
            view.update(cx, |view, cx| {
                if matches!(event, Event::ChooseSpecific) {
                    view.choose_specific_setup_broadcaster(side, window, cx);
                    return;
                }
                let Some(route) = view.active_setup_route_mut(side) else {
                    return;
                };
                match event {
                    Event::Random => route.selected = None,
                    Event::AllowOutOfRange(value) => route.allow_out_of_range = value,
                    Event::FavoritesOnly(value) => route.favorites_only = value,
                    Event::ChooseSpecific => {}
                }
                route.invalidate_estimate();
                view.refresh_setup_route(cx);
            });
        },
    )
}

/// What reusing the stealth account `index` at `address` on `chain_id` reveals, under its
/// select, with the address to copy. The text's tooltip holds the full privacy warning the
/// review repeats.
pub(super) fn account_reuse_line(
    id: &'static str,
    index: u32,
    address: Address,
    chain_id: u64,
) -> gpui::Div {
    reuse_line(
        id,
        format!(
            "Reuses #{index} · {} on {} and links this swap to its earlier activity there. No setup fee.",
            railgun_ui::short_address(&address),
            network_name(chain_id)
        ),
        address,
    )
}

/// A reuse warning's `text` under an account select, with the account's `address` to copy.
pub(super) fn reuse_line(id: &'static str, text: String, address: Address) -> gpui::Div {
    div()
        .debug_selector(move || id.into())
        .w_full()
        .flex()
        .items_start()
        .gap_1()
        .child(
            div()
                .id(id)
                .flex_1()
                .min_w_0()
                .flex()
                .items_start()
                .gap_1()
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .xsmall()
                        .flex_none()
                        .text_color(rgb(theme::WARNING)),
                )
                .child(
                    app_text(text)
                        .text_xs()
                        .min_w_0()
                        .text_color(rgb(theme::WARNING))
                        .whitespace_normal(),
                )
                .tooltip(|window, cx| Tooltip::new(ACCOUNT_REUSE_NOTE).build(window, cx)),
        )
        .child(
            clipboard_with_toast(
                SharedString::from(format!("{id}-copy")),
                address.to_checksum(None),
            )
            .xsmall()
            .tooltip("Copy address"),
        )
}

/// A new stealth account, the first entry of an account select and its default.
pub(super) fn new_account_item() -> SwapAccountSelectItem {
    SwapAccountSelectItem {
        operation: None,
        address: None,
        label: "New account (recommended)".into(),
    }
}

/// Under Receive to: why the form's network can't take Private balance. A sync or setup-fee
/// check under way resolves by itself and shows its progress in the price line instead.
pub(super) fn receive_to_problem(form: &SwapForm) -> Option<gpui::Div> {
    let Err(DeliveryProblem::Network {
        problem,
        syncing: false,
    }) = &form.delivery
    else {
        return None;
    };
    // Under the control, past the label and the row's gap.
    Some(
        error_alert("swap-receive-to-problem", problem.clone()).pl(rems(ACCOUNT_LABEL_WIDTH + 0.5)),
    )
}

/// The line under the stealth account select while a setup is quoted. A `problem` the user has
/// to solve is a danger Alert, like the one under Receive to.
/// A `pending` line shimmers: a fee being estimated, or a setup the order waits for.
pub(super) fn setup_fee_line(line: String, problem: bool, pending: bool) -> gpui::Div {
    if problem {
        return error_alert("swap-setup-problem", line);
    }
    if pending {
        return app_muted_shimmer_text(line).text_xs().whitespace_normal();
    }
    app_muted_text(line).text_xs().whitespace_normal()
}

/// The failure choice's hint card: paragraphs under the title, the two options' led by their
/// labels.
pub(super) fn shield_failure_card(text: &[(Option<String>, String)], window: &Window) -> gpui::Div {
    hint_card("If the shield fails", theme::INFO, window).children(text.iter().map(
        |(label, paragraph)| {
            div()
                .flex()
                .flex_col()
                .children(
                    label
                        .clone()
                        .map(|label| div().font_weight(FontWeight::MEDIUM).child(label)),
                )
                .child(div().whitespace_normal().child(paragraph.clone()))
        },
    ))
}
