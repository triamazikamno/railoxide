#![cfg(not(target_family = "wasm"))]

use gpui_kit::{
    AppContext as _, Context, Entity, Focusable as _, IntoElement, KeyBinding, KeyContext,
    Keystroke, NoAction, ParentElement as _, Render, Styled as _, TestAppContext,
    VisualTestContext, Window,
    component::{
        Root,
        input::{DeleteToPreviousWordStart, Input, InputState, Paste, SelectAll},
    },
    div,
};

#[path = "../src/browser/keymap.rs"]
mod keymap;

struct InputProbe {
    input: Entity<InputState>,
}

impl Render for InputProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        div().w_full().child(Input::new(&self.input))
    }
}

#[gpui_kit::test]
fn mac_editing_overrides_wasm_defaults(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        // Reproduce the relevant WASM defaults even on a native macOS test host.
        cx.bind_keys([
            KeyBinding::new("cmd-a", NoAction, Some("Input")),
            KeyBinding::new("ctrl-a", SelectAll, Some("Input")),
            KeyBinding::new("ctrl-backspace", DeleteToPreviousWordStart, Some("Input")),
        ]);
        keymap::init(cx, true);
    });
    let mut input = None;
    let window = cx.add_window(|window, cx| {
        let state = cx.new(|cx| InputState::new(window, cx));
        input = Some(state.clone());
        let probe = cx.new(|_| InputProbe { input: state });
        Root::new(probe, window, cx)
    });
    let input = input.unwrap();
    let cx = VisualTestContext::from_window(*window, cx).into_mut();
    cx.update(|window, cx| input.read(cx).focus_handle(cx).focus(window, cx));
    cx.refresh().unwrap();
    cx.run_until_parked();

    cx.simulate_input("alpha beta");
    cx.simulate_keystrokes("cmd-a");
    cx.simulate_input("gamma");
    assert_eq!(cx.read(|cx| input.read(cx).value()), "gamma");

    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("pre ");
    assert_eq!(cx.read(|cx| input.read(cx).value()), "pre gamma");

    cx.simulate_keystrokes("cmd-right ctrl-backspace");
    assert_eq!(cx.read(|cx| input.read(cx).value()), "pre gamm");
    cx.simulate_keystrokes("alt-backspace");
    assert_eq!(cx.read(|cx| input.read(cx).value()), "pre ");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(cx.read(|cx| input.read(cx).value()), "pre gamm");
    cx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(cx.read(|cx| input.read(cx).value()), "pre ");

    cx.simulate_keystrokes("cmd-a cmd-c");
    assert_eq!(
        cx.read_from_clipboard().unwrap().text().as_deref(),
        Some("pre ")
    );
    cx.simulate_keystrokes("cmd-x");
    assert_eq!(cx.read(|cx| input.read(cx).value()), "");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(cx.read(|cx| input.read(cx).value()), "pre ");
}

#[gpui_kit::test]
fn overrides_preserve_non_mac_bindings_and_browser_paste(cx: &TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.bind_keys([
            KeyBinding::new("ctrl-a", SelectAll, Some("Input")),
            KeyBinding::new("cmd-v", Paste, Some("Input")),
        ]);
        let bindings = cx.key_bindings();
        let input = [KeyContext::parse("Input").unwrap()];
        let select_all = [Keystroke::parse("ctrl-a").unwrap()];
        keymap::init(cx, false);
        let (matches, _) = bindings.borrow().bindings_for_input(&select_all, &input);
        assert!(matches[0].action().as_any().is::<SelectAll>());

        keymap::init(cx, true);
        let paste = [Keystroke::parse("cmd-v").unwrap()];
        let (matches, pending) = bindings.borrow().bindings_for_input(&paste, &input);
        assert!(
            matches.is_empty() && !pending,
            "Command-V must reach the DOM paste handler"
        );
        let select_all = [Keystroke::parse("cmd-a").unwrap()];
        let outside_input = [KeyContext::parse("GatewayView").unwrap()];
        let (matches, pending) = bindings
            .borrow()
            .bindings_for_input(&select_all, &outside_input);
        assert!(
            matches.is_empty() && !pending,
            "input overrides must not capture window shortcuts"
        );
    });
}
