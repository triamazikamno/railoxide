use super::*;
use gpui::VisualTestContext;
use gpui::{AppContext as _, Context, Entity, Render, TestAppContext};

struct NetworkProbe {
    open: bool,
    focus: FocusHandle,
    kind: NetworkStatusKind,
    query: TorExitIpQueryState,
    confirming: bool,
    rotating: bool,
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
        )
        .with_direct_rpc_endpoints(2);
        let query = self.query.clone();
        let confirming = self.confirming;
        let rotating = self.rotating;
        gpui_component::popover::Popover::new("network-probe")
            .p_0()
            .open(self.open)
            .trigger(NetworkStatusTrigger::new(
                network_status_pill("network-probe-trigger", true, &status, None, None)
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
                        rotating,
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
        rotating: false,
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
    let handle = cx.add_window(|window, cx| gpui_component::Root::new(first.clone(), window, cx));
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
    assert!(
        cx.debug_bounds("wallet-network-rpc-isolation-disclosure")
            .is_none()
    );
    assert!(
        cx.debug_bounds("wallet-network-direct-rpc-disclosure")
            .is_some()
    );
    click(cx, "wallet-network-new-tor-session");
    first.update(cx, |probe, cx| {
        assert!(probe.open, "session recovery keeps the popover open");
        assert_eq!(probe.intents, [NetworkStatusIntent::NewTorSession]);
        probe.intents.clear();
        probe.rotating = true;
        cx.notify();
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    click(cx, "wallet-network-new-tor-session");
    assert!(
        cx.read(|cx| first.read(cx).intents.is_empty()),
        "a pending rotation accepts no duplicate activation"
    );
    cx.simulate_keystrokes("escape");
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        cx.read(|cx| !first.read(cx).open),
        "dismissal stays available while rotating"
    );
    cx.simulate_keystrokes("enter");
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        cx.read(|cx| first.read(cx).open),
        "focus returns to the trigger, which reopens the popover"
    );
    first.update(cx, |probe, cx| {
        probe.rotating = false;
        cx.notify();
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
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
        assert_eq!(second.read(cx).intents, [] as [NetworkStatusIntent; 0]);
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
            cx.debug_bounds("wallet-network-rpc-isolation-disclosure")
                .is_some()
        );
        assert_eq!(
            cx.debug_bounds("wallet-network-direct-rpc-disclosure")
                .is_some(),
            kind == NetworkStatusKind::Proxy,
            "only Tor and proxy modes disclose direct endpoints"
        );
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
    let handle = cx.add_window(|window, cx| gpui_component::Root::new(view.clone(), window, cx));
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
