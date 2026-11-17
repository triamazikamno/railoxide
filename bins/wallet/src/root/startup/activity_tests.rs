use std::cell::Cell;

use gpui::{
    FocusHandle, InteractiveElement as _, KeyDownEvent, Keystroke, Modifiers, MouseButton,
    ScrollDelta, ScrollEvent, TestAppContext, TouchPhase, point,
};

use super::*;

struct ActivityObserverProbe {
    count: Rc<Cell<usize>>,
    stop_activity: Rc<Cell<bool>>,
    content_clicks: Rc<Cell<usize>>,
    root_focus: FocusHandle,
    dialog_focus: FocusHandle,
    dialog_open: bool,
    _keystroke_interceptor: Subscription,
}

impl Render for ActivityObserverProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        let count = Rc::clone(&self.count);
        let stop_activity = Rc::clone(&self.stop_activity);
        let observer = canvas(
            |_, _window, _cx| {},
            move |_, (), window, _cx| {
                let count = Rc::clone(&count);
                register_wallet_activity_listeners(
                    window,
                    Rc::new(move |_window, _cx| {
                        count.set(count.get() + 1);
                        stop_activity.get()
                    }),
                );
            },
        )
        .absolute()
        .size_full();
        let content_clicks = Rc::clone(&self.content_clicks);
        div()
            .debug_selector(|| "activity-probe".to_owned())
            .relative()
            .size_full()
            .track_focus(&self.root_focus)
            .child(observer)
            .child(div().size_full().on_mouse_down(
                MouseButton::Left,
                move |_event, _window, _cx| {
                    content_clicks.set(content_clicks.get() + 1);
                },
            ))
            .when(self.dialog_open, |this| {
                this.child(
                    div()
                        .debug_selector(|| "activity-dialog".to_owned())
                        .absolute()
                        .size_full()
                        .track_focus(&self.dialog_focus)
                        .occlude()
                        .on_key_down(|_, _, cx| cx.stop_propagation())
                        .on_mouse_move(|_, _, cx| cx.stop_propagation())
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_scroll(|_, _, cx| cx.stop_propagation()),
                )
            })
    }
}

#[gpui::test]
fn wallet_activity_observer_captures_content_and_dialog_events(cx: &mut TestAppContext) {
    let count = Rc::new(Cell::new(0));
    let probe_count = Rc::clone(&count);
    let (probe, cx) = cx.add_window_view(|window, cx| {
        let wallet_window = window.window_handle();
        let key_count = Rc::clone(&probe_count);
        let keystroke_interceptor = cx.intercept_keystrokes(move |_event, window, _cx| {
            if window.window_handle() == wallet_window {
                key_count.set(key_count.get() + 1);
            }
        });
        ActivityObserverProbe {
            count: probe_count,
            stop_activity: Rc::new(Cell::new(false)),
            content_clicks: Rc::new(Cell::new(0)),
            root_focus: cx.focus_handle(),
            dialog_focus: cx.focus_handle(),
            dialog_open: false,
            _keystroke_interceptor: keystroke_interceptor,
        }
    });
    cx.update(|window, app| {
        let focus = probe.read(app).root_focus.clone();
        window.focus(&focus, app);
        window.activate_window();
    });
    cx.refresh().expect("refresh test window");
    cx.run_until_parked();
    let content_position = cx
        .debug_bounds("activity-probe")
        .expect("probe bounds")
        .center();

    cx.simulate_mouse_move(content_position, None::<MouseButton>, Modifiers::none());
    cx.simulate_mouse_down(content_position, MouseButton::Left, Modifiers::none());
    cx.simulate_event(ScrollEvent {
        position: content_position,
        delta: ScrollDelta::Pixels(point(px(0.0), px(-10.0))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
        ..Default::default()
    });
    cx.simulate_event(KeyDownEvent {
        keystroke: Keystroke::parse("a").expect("valid test keystroke"),
        is_held: false,
        prefer_character_input: false,
    });
    assert_eq!(count.get(), 4);

    probe.update(cx, |probe, cx| {
        probe.dialog_open = true;
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, app| {
        let focus = probe.read(app).dialog_focus.clone();
        window.focus(&focus, app);
    });
    let dialog_position = cx
        .debug_bounds("activity-dialog")
        .expect("dialog bounds")
        .center();

    cx.simulate_mouse_move(dialog_position, None::<MouseButton>, Modifiers::none());
    cx.simulate_mouse_down(dialog_position, MouseButton::Left, Modifiers::none());
    cx.simulate_event(ScrollEvent {
        position: dialog_position,
        delta: ScrollDelta::Pixels(point(px(0.0), px(-10.0))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
        ..Default::default()
    });
    cx.simulate_event(KeyDownEvent {
        keystroke: Keystroke::parse("b").expect("valid test keystroke"),
        is_held: false,
        prefer_character_input: false,
    });
    assert_eq!(count.get(), 8);
}

#[gpui::test]
fn wallet_activity_observer_stops_expired_input_before_content(cx: &mut TestAppContext) {
    let count = Rc::new(Cell::new(0));
    let probe_count = Rc::clone(&count);
    let content_clicks = Rc::new(Cell::new(0));
    let probe_content_clicks = Rc::clone(&content_clicks);
    let (probe, cx) = cx.add_window_view(|_window, cx| ActivityObserverProbe {
        count: probe_count,
        stop_activity: Rc::new(Cell::new(true)),
        content_clicks: probe_content_clicks,
        root_focus: cx.focus_handle(),
        dialog_focus: cx.focus_handle(),
        dialog_open: false,
        _keystroke_interceptor: cx.intercept_keystrokes(|_, _, _| {}),
    });
    cx.update(|window, app| {
        let focus = probe.read(app).root_focus.clone();
        window.focus(&focus, app);
        window.activate_window();
    });
    cx.refresh().expect("refresh test window");
    cx.run_until_parked();
    let content_position = cx
        .debug_bounds("activity-probe")
        .expect("probe bounds")
        .center();

    cx.simulate_mouse_down(content_position, MouseButton::Left, Modifiers::none());

    assert_eq!(count.get(), 1);
    assert_eq!(content_clicks.get(), 0);
}

#[test]
fn network_context_transition_planner_retains_only_for_tor() {
    assert_eq!(
        network_context_plan(
            Some(WalletNetworkMode::Tor),
            WalletNetworkMode::Direct,
            false,
        ),
        NetworkContextPlan::RetainActiveAndBuild,
    );
    assert_eq!(
        network_context_plan(
            Some(WalletNetworkMode::Tor),
            WalletNetworkMode::Proxy,
            false,
        ),
        NetworkContextPlan::RetainActiveAndBuild,
    );
    assert_eq!(
        network_context_plan(
            Some(WalletNetworkMode::Direct),
            WalletNetworkMode::Tor,
            true,
        ),
        NetworkContextPlan::ReuseRetained,
    );
    assert_eq!(
        network_context_plan(Some(WalletNetworkMode::Tor), WalletNetworkMode::Tor, true),
        NetworkContextPlan::ReuseActive,
    );
    assert_eq!(
        network_context_plan(None, WalletNetworkMode::Direct, true),
        NetworkContextPlan::Build,
    );
}
