//! The route a public broadcaster submission runs on, as a progress dialog's title shows it: a
//! presence chip with the submission node's peer count, and a hover card that explains the
//! route. The wallet header's presence chips share its dot and label.

use gpui::{
    App, AppContext, Context, ElementId, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::badge::Badge;
use ui::theme::{self, APP_TEXT_SIZE};
use wallet_ops::{SUBMISSION_ROUTE_RECOVERY_LIMIT, SubmissionRouteProgress, SubmissionRouteState};

use super::chain_load::PresenceStatus;
use super::{count_label, rgb_with_alpha};

const ISOLATED_ROUTE_DETAIL: &str = "This operation's chain reads, PPOI submissions and broadcaster request use Tor circuits of their own, separate from the rest of the wallet and from other operations. The request is sent from a one-time Waku identity.";
const ROUTE_RECOVERY_DETAIL: &str = "If the route is lost, the wallet reconnects it on new circuits with the same identity. Submitting again after a timeout builds a new route with a new identity.";
/// Only Direct mode submits without a route of its own: proxy mode can't submit at all.
const NOT_ISOLATED_DETAIL: &str = "Network mode: Direct. This operation shares the wallet's connection and is not isolated from other activity. Turn on built-in Tor in Settings > Privacy to isolate it.";

pub(super) fn status_presence_text(
    label: impl Into<SharedString>,
    status: PresenceStatus,
) -> gpui::Div {
    div()
        .h(px(24.0))
        .min_w_0()
        .px_1()
        .flex()
        .items_center()
        .gap_1()
        .text_color(rgb(theme::TEXT))
        .child(status_presence_dot(status))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(label.into()),
        )
}

pub(super) fn status_presence_dot(status: PresenceStatus) -> gpui::Div {
    if status == PresenceStatus::Healthy {
        return healthy_presence_dot();
    }
    div()
        .size(px(7.0))
        .rounded_full()
        .bg(rgb(presence_status_color(status)))
}

fn healthy_presence_dot() -> gpui::Div {
    const SLOT_SIZE: f32 = 15.0;

    div()
        .relative()
        .size(px(SLOT_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .absolute()
                .top(px(3.0))
                .left(px(3.0))
                .size(px(9.0))
                .rounded_full()
                .bg(rgb_with_alpha(theme::SUCCESS, 0.38))
                .opacity(0.52),
        )
        .child(div().size(px(6.0)).rounded_full().bg(rgb(theme::SUCCESS)))
}

pub(super) const fn presence_status_color(status: PresenceStatus) -> u32 {
    match status {
        PresenceStatus::Healthy => theme::SUCCESS,
        PresenceStatus::Active => theme::WARNING,
        PresenceStatus::Error => theme::DANGER,
        PresenceStatus::Unknown => theme::TEXT_MUTED,
    }
}

/// What a route chip shows: its presence dot, its label, and the peer count of a ready route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) struct SubmissionRouteIndicator {
    pub(in crate::root) status: PresenceStatus,
    pub(in crate::root) label: &'static str,
    pub(in crate::root) peers: Option<usize>,
}

/// The chip of a submission whose route stands at `route`. Outside built-in Tor the operation
/// has no route of its own, whatever the route's state, and a submission that `ended` or a
/// `Closed` route reads closed, at whatever stage it ended.
pub(in crate::root) const fn submission_route_indicator(
    isolated: bool,
    route: SubmissionRouteProgress,
    ended: bool,
) -> SubmissionRouteIndicator {
    use PresenceStatus::{Active, Healthy, Unknown};
    use SubmissionRouteState::{Closed, Connecting, Preparing, Ready, Reconnecting};
    let (status, label, peers) = match (isolated, ended, route.state) {
        (false, _, _) => (Unknown, "Not isolated", None),
        (true, true, _) | (true, _, Closed) => (Unknown, "Route closed", None),
        (true, false, Preparing) => (Healthy, "Isolated route", None),
        (true, false, Connecting) => (Active, "Connecting", None),
        (true, false, Ready) => (Healthy, "Isolated route", Some(route.peers)),
        (true, false, Reconnecting) => (Active, "Reconnecting", None),
    };
    SubmissionRouteIndicator {
        status,
        label,
        peers,
    }
}

/// How ready a route is, the least ready lowest.
const fn route_readiness(state: SubmissionRouteState) -> u8 {
    match state {
        SubmissionRouteState::Reconnecting => 0,
        SubmissionRouteState::Connecting => 1,
        SubmissionRouteState::Preparing => 2,
        SubmissionRouteState::Ready => 3,
        // Closed routes are left out before they are combined.
        SubmissionRouteState::Closed => 4,
    }
}

/// Several running submissions' routes as one chip shows them: the least ready state, and the
/// peers of all of them together. `None` without any route.
fn combined_submission_route(
    routes: impl IntoIterator<Item = SubmissionRouteProgress>,
) -> Option<SubmissionRouteProgress> {
    routes
        .into_iter()
        .reduce(|combined, route| SubmissionRouteProgress {
            state: if route_readiness(route.state) < route_readiness(combined.state) {
                route.state
            } else {
                combined.state
            },
            peers: combined.peers + route.peers,
            ..combined
        })
}

/// What a route's hover card tells: whether the operation runs on routes of its own, whether
/// its submission ended, and each running route, under its network when several run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct SubmissionRouteCard {
    pub(in crate::root) isolated: bool,
    pub(in crate::root) ended: bool,
    pub(in crate::root) routes: Vec<(Option<String>, SubmissionRouteProgress)>,
}

impl SubmissionRouteCard {
    /// The chip of the card's routes, shown as one.
    pub(in crate::root) fn indicator(&self) -> SubmissionRouteIndicator {
        submission_route_indicator(
            self.isolated,
            combined_submission_route(self.routes.iter().map(|(_, route)| *route))
                .unwrap_or_default(),
            self.ended,
        )
    }
}

/// The route chip of `card`. Its hover card reads the card from `source` with `read`, and
/// follows `source` while open. The chip has no controls.
pub(in crate::root) fn render_submission_route_indicator<T: 'static>(
    id: impl Into<ElementId>,
    card: &SubmissionRouteCard,
    source: &Entity<T>,
    read: fn(&T, &App) -> Option<SubmissionRouteCard>,
) -> gpui::Stateful<gpui::Div> {
    let indicator = card.indicator();
    let text = status_presence_text(indicator.label, indicator.status);
    let source = source.clone();
    div()
        .id(id)
        .flex_none()
        // The count badge sits 3 px above the label, and a dialog clips its title row's top.
        .pt(px(3.0))
        .hoverable_tooltip(move |_window, cx| {
            let source = source.clone();
            cx.new(|cx| SubmissionRouteHoverCard::new(source, read, cx))
                .into()
        })
        .map(|chip| match indicator.peers {
            Some(peers) => chip.child(
                Badge::new()
                    .count(peers)
                    .color(rgb(theme::SUCCESS))
                    .child(text.pr(px(12.0)).into_any_element()),
            ),
            None => chip.child(text),
        })
}

struct SubmissionRouteHoverCard<T: 'static> {
    source: Entity<T>,
    read: fn(&T, &App) -> Option<SubmissionRouteCard>,
}

impl<T: 'static> SubmissionRouteHoverCard<T> {
    fn new(
        source: Entity<T>,
        read: fn(&T, &App) -> Option<SubmissionRouteCard>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        cx.observe(&source, |_this, _source, cx| cx.notify())
            .detach();
        Self { source, read }
    }
}

impl<T: 'static> Render for SubmissionRouteHoverCard<T> {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let app: &App = cx;
        let Some(card) = (self.read)(self.source.read(app), app) else {
            return div();
        };
        let indicator = card.indicator();
        let color = presence_status_color(indicator.status);

        div()
            .w(px(360.0))
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap_3()
            .text_size(APP_TEXT_SIZE)
            .text_color(rgb(theme::TEXT))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(status_presence_dot(indicator.status).flex_none())
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(color))
                            .child(indicator.label),
                    ),
            )
            .child(route_card_text(if card.isolated {
                ISOLATED_ROUTE_DETAIL
            } else {
                NOT_ISOLATED_DETAIL
            }))
            .when(card.isolated, |this| {
                this.child(render_route_card_routes(&card))
                    .child(route_card_text(ROUTE_RECOVERY_DETAIL))
            })
    }
}

fn route_card_text(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(12.0))
        .line_height(px(18.0))
        .text_color(rgb(theme::TEXT_MUTED))
        .child(text.into())
}

/// One route as rows of its status, peers and reconnects, or several as a line each under its
/// network. A closed route has no peers to count.
fn render_route_card_routes(card: &SubmissionRouteCard) -> gpui::Div {
    let routes = div().flex().flex_col().gap_1();
    match card.routes.as_slice() {
        [(_, route)] => routes
            .child(route_card_row(
                "Status",
                route_state_label(route.state, card.ended),
            ))
            .when(!route_closed(route.state, card.ended), |routes| {
                routes.child(route_card_row("Connected peers", route.peers.to_string()))
            })
            .child(route_card_row(
                "Reconnects used",
                reconnects_used(route.recoveries_used),
            )),
        several => routes.children(several.iter().map(|(network, route)| {
            let peers = if route_closed(route.state, card.ended) {
                String::new()
            } else {
                format!(", {}", count_label(route.peers, "peer"))
            };
            route_card_text(format!(
                "{}: {}{peers}, {} reconnects used",
                network.as_deref().unwrap_or_default(),
                route_state_label(route.state, card.ended),
                reconnects_used(route.recoveries_used),
            ))
        })),
    }
}

fn route_card_row(label: &'static str, value: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .text_size(px(12.0))
        .child(
            div()
                .min_w_0()
                .text_color(rgb(theme::TEXT_MUTED))
                .truncate()
                .child(label),
        )
        .child(
            div()
                .flex_none()
                .text_color(rgb(theme::TEXT))
                .child(value.into()),
        )
}

/// Whether a route's submission stopped using it, so that it has no live peers.
const fn route_closed(state: SubmissionRouteState, ended: bool) -> bool {
    ended || matches!(state, SubmissionRouteState::Closed)
}

const fn route_state_label(state: SubmissionRouteState, ended: bool) -> &'static str {
    match (route_closed(state, ended), state) {
        (true, _) | (false, SubmissionRouteState::Closed) => "Closed",
        (false, SubmissionRouteState::Preparing) => "Preparing",
        (false, SubmissionRouteState::Connecting) => "Connecting",
        (false, SubmissionRouteState::Ready) => "Ready",
        (false, SubmissionRouteState::Reconnecting) => "Reconnecting",
    }
}

fn reconnects_used(used: u32) -> String {
    format!("{used} of {SUBMISSION_ROUTE_RECOVERY_LIMIT}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Outside built-in Tor the operation shares the wallet's connection, so the chip never
    /// claims a route of its own or counts peers, in any state, ended or not.
    #[test]
    fn direct_mode_reads_not_isolated_in_every_state() {
        for state in [
            SubmissionRouteState::Preparing,
            SubmissionRouteState::Connecting,
            SubmissionRouteState::Ready,
            SubmissionRouteState::Reconnecting,
            SubmissionRouteState::Closed,
        ] {
            for ended in [false, true] {
                let route = SubmissionRouteProgress {
                    state,
                    peers: 3,
                    ..SubmissionRouteProgress::default()
                };
                let indicator = submission_route_indicator(false, route, ended);
                assert_eq!(indicator.label, "Not isolated");
                assert_eq!(indicator.peers, None);
            }
        }
        let closed = SubmissionRouteProgress {
            state: SubmissionRouteState::Closed,
            peers: 3,
            ..SubmissionRouteProgress::default()
        };
        let indicator = submission_route_indicator(true, closed, false);
        assert_eq!((indicator.label, indicator.peers), ("Route closed", None));
    }
}
