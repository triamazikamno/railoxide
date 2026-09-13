//! Network presentation shared by desktop and authenticated browser views.
//! Callers own observations, popover lifetimes, and every side effect.

use crate::controls::{app_button, app_muted_text, app_strong_text};
use crate::format::{
    format_compact_duration, format_compact_latency, format_decimal_byte_rate,
    format_decimal_bytes, format_relative_age,
};
use crate::theme::{self, APP_TEXT_LINE_HEIGHT, APP_TEXT_SIZE};
use gpui::{
    AnyElement, App, ElementId, FocusHandle, InteractiveElement, IntoElement, MouseButton,
    ParentElement, RenderOnce, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Selectable, Sizable, ThemeStyled,
    alert::Alert,
    button::{Button, ButtonVariants},
    spinner::Spinner,
};
use std::{fmt::Display, net::IpAddr, rc::Rc, sync::Arc, time::Duration};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkStatusKind {
    TorReady,
    TorReconnecting,
    TorDegraded,
    Proxy,
    Direct,
}

impl NetworkStatusKind {
    #[must_use]
    pub const fn is_tor(self) -> bool {
        matches!(
            self,
            Self::TorReady | Self::TorReconnecting | Self::TorDegraded
        )
    }
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::TorReady => "Tor",
            Self::TorReconnecting => "Tor reconnecting",
            Self::TorDegraded => "Tor degraded",
            Self::Proxy => "Proxy mode",
            Self::Direct => "Direct mode",
        }
    }
    #[must_use]
    pub const fn color(self) -> u32 {
        match self {
            Self::TorReady => theme::SUCCESS,
            Self::TorReconnecting => theme::WARNING,
            Self::TorDegraded => theme::DANGER,
            Self::Proxy => theme::PRIMARY,
            Self::Direct => theme::TEXT_MUTED,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkStatus {
    kind: NetworkStatusKind,
    detail: String,
    runtime_warning: bool,
}
impl NetworkStatus {
    #[must_use]
    pub const fn new(kind: NetworkStatusKind, detail: String, runtime_warning: bool) -> Self {
        Self {
            kind,
            detail,
            runtime_warning,
        }
    }
    #[must_use]
    pub const fn kind(&self) -> NetworkStatusKind {
        self.kind
    }
}

/// Aggregate observations only. Missing activity and measured zero are distinct.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct NetworkActivity {
    pub generation: u64,
    pub session_duration: Duration,
    pub downloaded_bytes: u64,
    pub recent_connection_sample_count: usize,
    pub recent_successful_sample_count: usize,
    pub successful_connections: u64,
    pub failed_connections: u64,
    pub median_setup_duration: Option<Duration>,
    pub last_activity_age: Option<Duration>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum TorExitIpQueryState {
    #[default]
    Idle,
    Querying,
    Success(IpAddr),
    Error(Arc<str>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkStatusIntent {
    NewTorSession,
    QueryExitIp,
    BeginReset,
    CancelReset,
    QuitAndReset,
}

/// Both frontends supply their own overlay and focus owner around this trigger.
#[must_use]
pub fn network_status_pill(
    id: impl Into<ElementId>,
    collapsed: bool,
    status: &NetworkStatus,
    activity: Option<&NetworkActivity>,
    rate: Option<u64>,
    expanded_width: gpui::Pixels,
) -> Button {
    let setup = activity
        .and_then(|activity| activity.median_setup_duration)
        .map_or_else(|| "--".to_owned(), format_compact_latency);
    Button::new(id)
        .text()
        .accessibility_label(format!("Desktop network: {}", status.kind.label()))
        .tooltip(format!("Desktop network: {}", status.kind.label()))
        .child(network_status_chip(
            collapsed,
            status.kind.color(),
            status.kind.label(),
            expanded_width,
            &setup,
            rate,
            status.kind.is_tor(),
            status.kind == NetworkStatusKind::TorReconnecting,
        ))
}

/// GPUI Component 0.6's Button replaces externally supplied focus handles during render.
/// This wrapper owns a stable focus target while keeping the shared button's appearance.
#[derive(IntoElement)]
pub struct NetworkStatusTrigger {
    button: Button,
    focus: FocusHandle,
    label: SharedString,
}

impl NetworkStatusTrigger {
    #[must_use]
    pub fn new(button: Button, focus: &FocusHandle, status: &NetworkStatus) -> Self {
        Self {
            button,
            focus: focus.clone(),
            label: format!("Desktop network: {}", status.kind.label()).into(),
        }
    }
}

impl Selectable for NetworkStatusTrigger {
    fn selected(mut self, selected: bool) -> Self {
        self.button = self.button.selected(selected);
        self
    }

    fn is_selected(&self) -> bool {
        self.button.is_selected()
    }
}

impl RenderOnce for NetworkStatusTrigger {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .id("network-status-trigger-focus")
            .track_focus(&self.focus)
            .tab_stop(true)
            .role(gpui::accesskit::Role::Button)
            .aria_label(self.label)
            .rounded_md()
            .when(self.focus.is_focused(window), |this| {
                this.focus_ring_style(window, cx)
            })
            .child(self.button.tab_stop(false).role(None))
    }
}

/// The overlay owns the viewport and scrolls the full content, including confirmation controls.
#[must_use]
pub fn network_status_scroll(content: gpui::Div, window: &Window) -> gpui::Stateful<gpui::Div> {
    let rem = window.rem_size();
    let viewport = window.viewport_size();
    div()
        .id("network-status-scroll")
        .debug_selector(|| "network-status-scroll".into())
        .w((rem * 25.0).min((viewport.width - rem).max(px(1.0))))
        .max_h((viewport.height - rem).max(px(1.0)))
        .overflow_y_scroll()
        .child(content.w_full().p_3().flex_none())
}

fn rgb_with_alpha(color: u32, alpha: f32) -> gpui::Rgba {
    let mut value = rgb(color);
    value.a = alpha;
    value
}

fn format_optional_number<T: Display>(value: Option<T>) -> String {
    value.map_or_else(|| "--".to_owned(), |value| value.to_string())
}

fn format_recent_reliability(successful: Option<usize>, attempts: Option<usize>) -> String {
    let (Some(successful), Some(attempts)) = (successful, attempts) else {
        return "--".to_owned();
    };
    if attempts == 0 {
        return "--".to_owned();
    }

    let tenths = successful
        .saturating_mul(1_000)
        .saturating_add(attempts / 2)
        / attempts;
    let whole = tenths / 10;
    let fractional = tenths % 10;
    if fractional == 0 {
        format!("{whole}%")
    } else {
        format!("{whole}.{fractional}%")
    }
}

fn tor_activity_stat_row(label: impl Into<SharedString>, value: String) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .items_start()
        .justify_between()
        .gap_3()
        .child(app_muted_text(label).flex_none())
        .child(
            app_strong_text(value)
                .min_w(px(0.0))
                .flex_1()
                .text_align(gpui::TextAlign::Right)
                .whitespace_normal(),
        )
}

fn tor_activity_connections_row(successful: Option<u64>, failed: Option<u64>) -> gpui::Div {
    let has_activity = successful.is_some();
    div()
        .w_full()
        .flex()
        .items_start()
        .justify_between()
        .gap_3()
        .child(
            app_muted_text("Connections (succeeded / failed)")
                .debug_selector(|| "network-connection-label".into())
                .min_w(px(0.0))
                .flex_1()
                .whitespace_normal(),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .gap_1()
                .whitespace_nowrap()
                .child(
                    app_strong_text(format_optional_number(successful))
                        .debug_selector(|| "network-connection-successes".into())
                        .text_color(rgb(if has_activity {
                            theme::SUCCESS
                        } else {
                            theme::TEXT_MUTED
                        }))
                        .flex_none(),
                )
                .child(app_muted_text("/").flex_none())
                .child(
                    app_strong_text(format_optional_number(failed))
                        .debug_selector(|| "network-connection-failures".into())
                        .text_color(rgb(if has_activity {
                            theme::DANGER
                        } else {
                            theme::TEXT_MUTED
                        }))
                        .flex_none(),
                ),
        )
}

fn network_status_chip(
    collapsed: bool,
    color: u32,
    label: &'static str,
    expanded_width: gpui::Pixels,
    setup_label: &str,
    rate: Option<u64>,
    tor_metrics_visible: bool,
    tor_reconnecting: bool,
) -> gpui::AnyElement {
    if collapsed {
        return div()
            .id("wallet-network-status-pill-collapsed")
            .h(gpui::rems(32.0 / 16.0))
            .px_2()
            .flex()
            .items_center()
            .justify_center()
            .rounded_lg()
            .border_1()
            .border_color(rgb(color))
            .bg(rgb_with_alpha(color, 0.08))
            .text_color(rgb(color))
            .cursor_pointer()
            .hover(|this| this.bg(rgb_with_alpha(color, 0.14)))
            .child(
                Icon::empty()
                    .path(crate::icons::tor_status_icon_path())
                    .small()
                    .text_color(rgb(color)),
            )
            .into_any_element();
    }

    if !tor_metrics_visible {
        return div()
            .id("wallet-network-status-pill")
            .h_7()
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(rgb(color))
            .bg(rgb_with_alpha(color, 0.08))
            .text_color(rgb(color))
            .cursor_pointer()
            .hover(|this| this.bg(rgb_with_alpha(color, 0.14)))
            .child(
                Icon::empty()
                    .path(crate::icons::tor_status_icon_path())
                    .small()
                    .text_color(rgb(color)),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(gpui::rems(13.0 / 16.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .line_height(gpui::relative(APP_TEXT_LINE_HEIGHT))
                    .text_color(rgb(color))
                    .child(label),
            )
            .into_any_element();
    }

    let displayed_label = if tor_reconnecting { "Tor" } else { label };

    div()
        .id("wallet-network-status-pill")
        .h(gpui::rems(42.0 / 16.0))
        .w(expanded_width)
        .p_2()
        .flex()
        .items_center()
        .gap_2()
        .rounded_lg()
        .border_1()
        .border_color(rgb(color))
        .bg(rgb_with_alpha(color, 0.08))
        .text_color(rgb(color))
        .cursor_pointer()
        .hover(|this| this.bg(rgb_with_alpha(color, 0.14)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Icon::empty()
                        .path(crate::icons::tor_status_icon_path())
                        .small()
                        .flex_none()
                        .text_color(rgb(color)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .when(!tor_reconnecting, gpui::Styled::flex_1)
                                .min_w_0()
                                .truncate()
                                .text_size(gpui::rems(13.0 / 16.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .line_height(gpui::relative(APP_TEXT_LINE_HEIGHT))
                                .text_color(rgb(color))
                                .child(displayed_label),
                        )
                        .children(tor_reconnecting.then(|| {
                            div().flex_none().child(
                                Spinner::new()
                                    .icon(IconName::LoaderCircle)
                                    .color(rgb(color).into())
                                    .xsmall(),
                            )
                        })),
                ),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .items_end()
                .gap(gpui::rems(2.0 / 16.0))
                .text_size(gpui::rems(11.0 / 16.0))
                // Numeric readouts retain the native compact line box; prose uses shared leading.
                .line_height(gpui::rems(11.0 / 16.0))
                .text_color(rgb(color))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .whitespace_nowrap()
                        .child(
                            Icon::empty()
                                .path(crate::icons::arrow_right_left_icon_path())
                                .size(gpui::rems(9.0 / 16.0)),
                        )
                        .child(setup_label.to_owned()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .whitespace_nowrap()
                        .child(
                            Icon::empty()
                                .path(crate::icons::arrow_down_to_line_icon_path())
                                .size(gpui::rems(9.0 / 16.0)),
                        )
                        .child(format_decimal_byte_rate(rate)),
                ),
        )
        .into_any_element()
}

pub fn network_status_popover_content(
    health: &NetworkStatus,
    error: Option<Arc<str>>,
    exit_ip_query: TorExitIpQueryState,
    reset_confirming: bool,
    activity: Option<&NetworkActivity>,
    download_rate: Option<u64>,
    on_intent: impl Fn(NetworkStatusIntent, &mut Window, &mut App) + 'static,
    copy_control: Option<AnyElement>,
) -> gpui::Div {
    let color = health.kind.color();
    let on_intent = Rc::new(on_intent);
    let session_intent = on_intent.clone();
    let query_intent = on_intent.clone();
    let reset_intent = on_intent.clone();
    let cancel_reset_intent = on_intent.clone();
    let confirm_reset_intent = on_intent;
    let exit_ip_querying = matches!(exit_ip_query, TorExitIpQueryState::Querying);
    let generation = activity.map(|snapshot| snapshot.generation);
    let session_duration = activity.map(|snapshot| snapshot.session_duration);
    let downloaded_bytes = activity.map(|snapshot| snapshot.downloaded_bytes);
    let recent_connection_sample_count =
        activity.map(|snapshot| snapshot.recent_connection_sample_count);
    let recent_successful_sample_count =
        activity.map(|snapshot| snapshot.recent_successful_sample_count);
    let successful_connections = activity.map(|snapshot| snapshot.successful_connections);
    let failed_connections = activity.map(|snapshot| snapshot.failed_connections);
    let median_setup_duration = activity.and_then(|snapshot| snapshot.median_setup_duration);
    let latency_label =
        median_setup_duration.map_or_else(|| "--".to_owned(), format_compact_latency);
    let last_activity_age = activity.and_then(|snapshot| snapshot.last_activity_age);
    div()
        .w(gpui::rems(300.0 / 16.0))
        .flex()
        .flex_col()
        .gap_3()
        .text_size(APP_TEXT_SIZE)
        .line_height(gpui::relative(APP_TEXT_LINE_HEIGHT))
        .text_color(rgb(theme::TEXT))
        .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
            cx.stop_propagation();
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Icon::empty().path(crate::icons::tor_status_icon_path())
                        .small()
                        .text_color(rgb(color)),
                )
                .child(
                    app_strong_text(health.kind.label())
                        .text_size(gpui::rems(14.0 / 16.0))
                        .text_color(rgb(color)),
                ),
        )
        .when(!health.kind.is_tor(), |this| {
            this.child(
                div()
                    .text_size(gpui::rems(12.0 / 16.0))
                    .line_height(gpui::relative(APP_TEXT_LINE_HEIGHT))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(health.detail.clone()),
            )
        })
        .when_some(error, |this, error| {
            this.child(Alert::error("wallet-network-status-error", error.to_string()).small())
        })
        .when(health.kind.is_tor(), |this| {
            this.when(
                health.runtime_warning,
                |this| {
                    this.child(
                        Alert::warning(
                            "wallet-network-runtime-warning",
                            "Tor is degraded. Try a new Tor session to reconnect.",
                        )
                        .small(),
                    )
                },
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(tor_activity_stat_row(
                                "Session ID",
                                generation.map_or_else(|| "--".to_owned(), |generation| {
                                    format!("#{generation}")
                                }),
                            ))
                            .child(tor_activity_stat_row(
                                "Session duration",
                                session_duration
                                    .map_or_else(|| "--".to_owned(), format_compact_duration),
                            ))
                            .child(tor_activity_stat_row(
                                "Download rate",
                                format_decimal_byte_rate(download_rate),
                            ))
                            .child(tor_activity_stat_row(
                                "Downloaded this session",
                                downloaded_bytes.map_or_else(|| "--".to_owned(), format_decimal_bytes),
                            ))
                            .child(tor_activity_stat_row("Latency", latency_label))
                            .child(tor_activity_stat_row(
                                "Recent reliability",
                                format_recent_reliability(
                                    recent_successful_sample_count,
                                    recent_connection_sample_count,
                                ),
                            ))
                            .child(tor_activity_connections_row(
                                successful_connections,
                                failed_connections,
                            ))
                            .child(tor_activity_stat_row(
                                "Last activity",
                                last_activity_age
                                    .map_or_else(|| "--".to_owned(), format_relative_age),
                            )),
                    )
                    .child(
                        app_button("wallet-network-new-tor-session", "New Tor session")
.debug_selector(|| "wallet-network-new-tor-session".into())
                            .w_full()
                            .primary()
                            .outline()
                            .small()
                            .on_click(move |_event, window, cx| {
                                cx.stop_propagation();
                                session_intent(NetworkStatusIntent::NewTorSession, window, cx);
                            }),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .border_t_1()
                    .border_color(rgb(theme::BORDER_SUBTLE))
                    .pt_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(gpui::rems(12.0 / 16.0))
                            .line_height(gpui::relative(APP_TEXT_LINE_HEIGHT))
                            .text_color(rgb(theme::TEXT_SUBTLE))
                            .child(
                                "Contacts https://check.torproject.org/api/ip through Tor.",
                            ),
                    )
                    .child(
                        app_button(
                            "wallet-network-query-exit-ip",
                            if exit_ip_querying {
                                "Querying..."
                            } else {
                                "Query exit IP"
                            },
                        )
                        .debug_selector(|| "wallet-network-query-exit-ip".into())
                        .w_full()
                        .outline()
                        .small()
                        .loading(exit_ip_querying)
                        .disabled(exit_ip_querying)
                        .on_click(move |_event, window, cx| {
                            cx.stop_propagation();
                            query_intent(NetworkStatusIntent::QueryExitIp, window, cx);
                        }),
                    )
                    .when(!matches!(exit_ip_query, TorExitIpQueryState::Idle), |this| {
                        this.child(match exit_ip_query {
                            TorExitIpQueryState::Idle => div().into_any_element(),
                            TorExitIpQueryState::Querying => div()
                                .text_size(gpui::rems(12.0 / 16.0))
                                .line_height(gpui::relative(APP_TEXT_LINE_HEIGHT))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child("Querying exit IP through Tor...")
                                .into_any_element(),
                            TorExitIpQueryState::Success(ip) => div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(gpui::rems(12.0 / 16.0))
                                .line_height(gpui::relative(APP_TEXT_LINE_HEIGHT))
                                .text_color(rgb(theme::SUCCESS))
                                .child(format!("Exit IP: {ip}"))
                                .children(copy_control)
                                .into_any_element(),
                            TorExitIpQueryState::Error(error) => div()
                                .text_size(gpui::rems(12.0 / 16.0))
                                .line_height(gpui::relative(APP_TEXT_LINE_HEIGHT))
                                .text_color(rgb(theme::DANGER))
                                .child(error.to_string())
                                .into_any_element(),
                        })
                    }),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .when(!reset_confirming, |this| {
                        this.border_t_1()
                            .border_color(rgb(theme::BORDER_SUBTLE))
                            .pt_3()
                    })
                    .when(reset_confirming, |this| {
                        this.rounded_md()
                            .border_1()
                            .border_color(rgb(theme::DANGER))
                            .bg(rgb_with_alpha(theme::DANGER, 0.08))
                            .p(gpui::rems(10.0 / 16.0))
                    })
                    .child(
                        div()
                            .text_size(gpui::rems(12.0 / 16.0))
                            .line_height(gpui::relative(APP_TEXT_LINE_HEIGHT))
                            .text_color(rgb(if reset_confirming {
                                theme::DANGER
                            } else {
                                theme::TEXT_SUBTLE
                            }))
                            .child(if reset_confirming {
                                "The wallet closes now and rebuilds its Tor connections when you reopen it. Only Tor's cached data is cleared."
                            } else {
                                "Clears Tor's cached relay data and reconnects from scratch on next launch. Your wallet, keys, and transactions are untouched."
                            }),
                    )
                    .when(!reset_confirming, |this| {
                        this.child(
                            app_button("wallet-network-reset-tor-state", "Reset Tor state")
.debug_selector(|| "wallet-network-reset-tor-state".into())
                                .w_full()
                                .outline()
                                .small()
                                .danger()
                                .on_click(move |_event, window, cx| {
                                    cx.stop_propagation();
                                    reset_intent(NetworkStatusIntent::BeginReset, window, cx);
                                }),
                        )
                    })
                    .when(reset_confirming, |this| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    app_button("wallet-network-cancel-tor-reset", "Cancel")
.debug_selector(|| "wallet-network-cancel-tor-reset".into())
                                        .outline()
                                        .small()
                                        .on_click(move |_event, window, cx| {
                                            cx.stop_propagation();
                                            cancel_reset_intent(NetworkStatusIntent::CancelReset, window, cx);
                                        }),
                                )
                                .child(
                                    app_button("wallet-network-confirm-tor-reset", "Quit and reset")
.debug_selector(|| "wallet-network-confirm-tor-reset".into())
                                        .small()
                                        .danger()
                                        .on_click(move |_event, window, cx| {
                                            cx.stop_propagation();
                                            confirm_reset_intent(NetworkStatusIntent::QuitAndReset, window, cx);
                                        }),
                                ),
                        )
                    }),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::VisualTestContext;
    use gpui::{AppContext as _, Context, Entity, Render, TestAppContext};

    struct NetworkProbe {
        open: bool,
        focus: FocusHandle,
        kind: NetworkStatusKind,
        query: TorExitIpQueryState,
        confirming: bool,
        intents: Vec<NetworkStatusIntent>,
    }

    impl Render for NetworkProbe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            let owner = cx.entity();
            let opening_owner = owner.clone();
            let status = NetworkStatus::new(
                self.kind,
                String::new(),
                self.kind == NetworkStatusKind::TorDegraded,
            );
            let query = self.query.clone();
            let confirming = self.confirming;
            gpui_component::popover::Popover::new("network-probe")
                .p_0()
                .open(self.open)
                .trigger(NetworkStatusTrigger::new(
                    network_status_pill(
                        "network-probe-trigger",
                        true,
                        &status,
                        None,
                        None,
                        px(0.0),
                    )
                    .debug_selector(|| "network-probe-trigger".into()),
                    &self.focus,
                    &status,
                ))
                .on_open_change(move |open, window, cx| {
                    opening_owner.update(cx, |probe, cx| {
                        probe.open = *open;
                        if !open {
                            probe.focus.focus(window, cx);
                        }
                        cx.notify();
                    });
                })
                .content(move |_, window, _| {
                    let owner = owner.clone();
                    network_status_scroll(
                        network_status_popover_content(
                            &status,
                            None,
                            query.clone(),
                            confirming,
                            Some(&NetworkActivity {
                                successful_connections: 9999,
                                failed_connections: 9999,
                                ..NetworkActivity::default()
                            }),
                            None,
                            move |intent, _, cx| {
                                owner.update(cx, |probe, cx| {
                                    probe.intents.push(intent);
                                    match intent {
                                        NetworkStatusIntent::QueryExitIp => {
                                            probe.query = TorExitIpQueryState::Querying;
                                        }
                                        NetworkStatusIntent::BeginReset => probe.confirming = true,
                                        NetworkStatusIntent::CancelReset => {
                                            probe.confirming = false;
                                        }
                                        _ => {}
                                    }
                                    cx.notify();
                                });
                            },
                            None,
                        ),
                        window,
                    )
                })
        }
    }

    fn probe(cx: &mut App) -> Entity<NetworkProbe> {
        cx.new(|cx| NetworkProbe {
            open: false,
            focus: cx.focus_handle(),
            kind: NetworkStatusKind::TorDegraded,
            query: TorExitIpQueryState::Idle,
            confirming: false,
            intents: Vec::new(),
        })
    }

    fn click(cx: &mut VisualTestContext, id: &'static str) {
        let bounds = cx.debug_bounds(id).expect("network control is rendered");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    #[gpui::test]
    fn shared_popover_routes_only_explicit_actions_to_its_owner(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let first = cx.update(probe);
        let second = cx.update(probe);
        let handle =
            cx.add_window(|window, cx| gpui_component::Root::new(first.clone(), window, cx));
        let cx = VisualTestContext::from_window(*handle, cx).into_mut();
        cx.simulate_resize(gpui::size(px(1000.0), px(1400.0)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        click(cx, "network-probe-trigger");
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.read(|cx| !first.read(cx).open));
        cx.simulate_keystrokes("enter");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.read(|cx| first.read(cx).open));
        assert!(cx.read(|cx| first.read(cx).intents.is_empty()));
        click(cx, "wallet-network-new-tor-session");
        first.update(cx, |probe, _| {
            assert!(probe.open, "session recovery keeps the popover open");
            assert_eq!(probe.intents, [NetworkStatusIntent::NewTorSession]);
            probe.intents.clear();
        });
        click(cx, "wallet-network-query-exit-ip");
        click(cx, "wallet-network-query-exit-ip");
        cx.read(|cx| {
            assert_eq!(first.read(cx).intents, [NetworkStatusIntent::QueryExitIp]);
            assert_eq!(second.read(cx).query, TorExitIpQueryState::Idle);
        });
        click(cx, "wallet-network-reset-tor-state");
        cx.read(|cx| {
            assert!(first.read(cx).confirming);
            assert!(!second.read(cx).confirming);
        });
        assert!(
            cx.debug_bounds("wallet-network-confirm-tor-reset")
                .is_some()
        );
        click(cx, "wallet-network-cancel-tor-reset");
        assert!(cx.read(|cx| {
            !first
                .read(cx)
                .intents
                .contains(&NetworkStatusIntent::QuitAndReset)
        }));
        click(cx, "wallet-network-reset-tor-state");
        click(cx, "wallet-network-confirm-tor-reset");
        cx.read(|cx| {
            assert_eq!(
                first.read(cx).intents.last(),
                Some(&NetworkStatusIntent::QuitAndReset)
            );
            assert!(second.read(cx).intents.is_empty());
        });
        for kind in [NetworkStatusKind::Proxy, NetworkStatusKind::Direct] {
            first.update(cx, |probe, cx| {
                probe.kind = kind;
                cx.notify();
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            assert!(cx.debug_bounds("wallet-network-new-tor-session").is_none());
            assert!(cx.debug_bounds("wallet-network-query-exit-ip").is_none());
            assert!(
                cx.debug_bounds("wallet-network-confirm-tor-reset")
                    .is_none()
            );
        }
    }

    #[gpui::test]
    fn four_digit_connection_counts_fit_at_normal_and_enlarged_scales(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let view = cx.update(probe);
        let handle =
            cx.add_window(|window, cx| gpui_component::Root::new(view.clone(), window, cx));
        let cx = VisualTestContext::from_window(*handle, cx).into_mut();
        cx.simulate_resize(gpui::size(px(1000.0), px(1400.0)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        click(cx, "network-probe-trigger");
        for (width, rem) in [(1000.0, 16.0), (320.0, 16.0), (320.0, 24.0)] {
            cx.simulate_resize(gpui::size(px(width), px(1400.0)));
            cx.update(|window, cx| {
                window.set_rem_size(px(rem));
                window.draw(cx).clear(cx);
            });
            let overlay = cx.debug_bounds("network-status-scroll").unwrap();
            let label = cx.debug_bounds("network-connection-label").unwrap();
            let successes = cx.debug_bounds("network-connection-successes").unwrap();
            let failures = cx.debug_bounds("network-connection-failures").unwrap();
            assert!(overlay.size.width <= px(width));
            assert!(
                label.right() < successes.left(),
                "label and counters must not overlap"
            );
            assert!(
                failures.right() < overlay.right(),
                "four-digit counters stay inside the popover"
            );
        }
    }

    #[test]
    fn recent_reliability_formatter_handles_missing_zero_and_rounded_percentages() {
        assert_eq!(format_recent_reliability(None, None), "--");
        assert_eq!(format_recent_reliability(Some(1), Some(0)), "--");
        assert_eq!(format_recent_reliability(Some(0), Some(64)), "0%");
        assert_eq!(format_recent_reliability(Some(63), Some(64)), "98.4%");
        assert_eq!(format_recent_reliability(Some(64), Some(64)), "100%");
    }
}
