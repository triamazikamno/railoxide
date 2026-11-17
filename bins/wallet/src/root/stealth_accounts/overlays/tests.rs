use super::*;
use gpui::Render;
use gpui_component::Root;
use std::{cell::Cell, rc::Rc};

struct DialogHost;

impl Render for DialogHost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full()
    }
}

#[gpui::test]
fn token_picker_selects_known_tokens_and_accepts_custom_addresses(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(ui::theme::apply_zenburn_component_theme);
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| DialogHost);
        Root::new(view, window, cx)
    });
    let registry = wallet_ops::settings::build_effective_token_registry(
        &wallet_ops::settings::WalletSettings::default(),
    )
    .unwrap();
    let known = registry
        .tokens
        .values()
        .find(|token| token.chain_id == 1 && token.symbol == "WETH")
        .unwrap()
        .token_address
        .parse::<alloy::primitives::Address>()
        .unwrap();
    let custom = alloy::primitives::Address::repeat_byte(0x42);
    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    };
    for (width, height, font_size) in [(1024., 900., 14.), (640., 480., 20.)] {
        cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(height)));
        let picker = cx.update(|window, cx| {
            gpui_component::Theme::global_mut(cx).font_size = gpui::px(font_size);
            gpui_component::Theme::sync_base(cx);
            let picker = cx.new(|cx| TokenPicker::new(&registry, 1, window, cx));
            let content = picker.clone();
            window.open_wallet_dialog(cx, move |dialog, window, _| {
                dialog
                    .title("Add token to account #0")
                    .w((window.viewport_size().width * 0.92)
                        .min(rems(30.).to_pixels(window.rem_size())))
                    .max_h(window.viewport_size().height * 0.9)
                    .child(content.clone())
            });
            picker.read(cx).focus_handle(cx).focus(window, cx);
            window.draw(cx).clear(cx);
            picker
        });
        draw(cx);
        cx.update(|window, cx| picker.read(cx).focus_handle(cx).focus(window, cx));
        draw(cx);
        cx.simulate_keystrokes("enter");
        draw(cx);
        cx.simulate_input("WETH");
        draw(cx);
        cx.simulate_keystrokes("enter");
        draw(cx);
        cx.update(|_, cx| assert_eq!(picker.read(cx).token(cx), Ok(known)));

        cx.update(|window, cx| picker.read(cx).focus_handle(cx).focus(window, cx));
        cx.simulate_keystrokes("enter");
        draw(cx);
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        });
        cx.simulate_input("custom");
        draw(cx);
        cx.simulate_keystrokes("enter");
        draw(cx);
        cx.update(|window, cx| {
            assert!(
                picker.read(cx).token(cx).is_err(),
                "The old known token must not remain selected"
            );
            window.draw(cx).clear(cx);
        });
        cx.simulate_input(&custom.to_checksum(None));
        cx.update(|_, cx| assert_eq!(picker.read(cx).token(cx), Ok(custom)));

        cx.update(|window, cx| picker.read(cx).focus_handle(cx).focus(window, cx));
        cx.simulate_keystrokes("enter");
        draw(cx);
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        });
        cx.simulate_input(&known.to_checksum(None));
        draw(cx);
        cx.simulate_keystrokes("enter");
        draw(cx);
        cx.update(|_, cx| assert_eq!(picker.read(cx).token(cx), Ok(known)));
        cx.update(gpui_component::WindowExt::close_dialog);
    }
}

#[gpui::test]
fn restore_dialog_keeps_the_form_and_review_reachable(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(ui::theme::apply_zenburn_component_theme);
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| DialogHost);
        Root::new(view, window, cx)
    });
    cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
    let reviewed = Rc::new(Cell::new(false));
    let on_review = reviewed.clone();
    let (start, count) = cx.update(|window, cx| {
        let start = cx.new(|cx| InputState::new(window, cx));
        let count = cx.new(|cx| InputState::new(window, cx).default_value("200"));
        let index = start.clone();
        let account_count = count.clone();
        window.open_wallet_dialog(cx, move |dialog, window, cx| {
            let on_review = on_review.clone();
            restore_dialog(dialog, &index, &account_count, false, window, cx).on_ok(
                move |_, _, _| {
                    on_review.set(true);
                    false
                },
            )
        });
        start.read(cx).focus_handle(cx).focus(window, cx);
        window.draw(cx).clear(cx);
        (start, count)
    });
    cx.simulate_input("12");
    cx.update(|window, cx| {
        assert_eq!(start.read(cx).value(), "12");
        window.draw(cx).clear(cx);
    });
    let review = cx
        .debug_bounds("stealth-restore-review")
        .expect("Review restore button");
    cx.simulate_click(review.center(), gpui::Modifiers::none());
    assert!(!reviewed.get(), "An invalid count must disable Review");
    cx.update(|window, cx| count.read(cx).focus_handle(cx).focus(window, cx));
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-a"
    } else {
        "ctrl-a"
    });
    cx.simulate_input("64");
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let review = cx.debug_bounds("stealth-restore-review").unwrap();
    cx.simulate_click(review.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(
        reviewed.get(),
        "Restore controls must be visible and clickable below the description"
    );

    reviewed.set(false);
    cx.simulate_resize(gpui::size(gpui::px(640.), gpui::px(480.)));
    cx.update(|window, cx| {
        gpui_component::Theme::global_mut(cx).font_size = gpui::px(20.);
        gpui_component::Theme::sync_base(cx);
        window.draw(cx).clear(cx);
    });
    cx.simulate_event(gpui::ScrollEvent {
        position: gpui::point(gpui::px(320.), gpui::px(300.)),
        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(-2000.))),
        ..Default::default()
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let review = cx
        .debug_bounds("stealth-restore-review")
        .expect("Review after scrolling");
    cx.simulate_click(review.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(
        reviewed.get(),
        "The bounded dialog must scroll to Review at enlarged scale"
    );
}

#[test]
fn restore_range_respects_batch_and_derivation_limits() {
    assert_eq!(restore_range(" 12 ", "64"), Ok(12..76));
    assert_eq!(restore_range("2147483647", "1"), Ok(2147483647..2147483648));
    for (start, count) in [
        ("0", "0"),
        ("0", "65"),
        ("0", "200"),
        ("0", ""),
        ("0", "4294967295"),
        ("-1", "1"),
        ("", "1"),
        ("2147483648", "1"),
        ("2147483647", "2"),
    ] {
        assert!(restore_range(start, count).is_err(), "{start}, {count}");
    }
}
