use gpui_kit::{
    App, KeyBinding, NoAction,
    component::input::{
        Backspace, Copy, Cut, DeleteToBeginningOfLine, DeleteToEndOfLine, DeleteToNextWordEnd,
        DeleteToPreviousWordStart, Enter, Indent, MoveEnd, MoveHome, MoveToEnd, MoveToNextWord,
        MoveToPreviousWord, MoveToStart, Outdent, Redo, Replace, Search, SelectAll, SelectToEnd,
        SelectToEndOfLine, SelectToNextWordEnd, SelectToPreviousWordStart, SelectToStart,
        SelectToStartOfLine, ToggleCodeActions, Undo,
    },
};

const INPUT: Option<&str> = Some("Input");

/// Register after Kit initialization so these bindings override its WASM defaults.
pub(super) fn init(cx: &mut App, is_mac: bool) {
    if !is_mac {
        return;
    }

    // gpui-base 0.6.0 selects desktop bindings with target_os. WASM therefore
    // gets the non-Mac bindings even in a Mac browser. Disable those first so
    // native Control shortcuts cannot fall through to a different edit.
    cx.bind_keys(
        [
            "ctrl-a",
            "ctrl-backspace",
            "ctrl-delete",
            "ctrl-enter",
            "ctrl-]",
            "ctrl-[",
            "ctrl-shift-left",
            "ctrl-shift-right",
            "ctrl-c",
            "ctrl-x",
            "ctrl-v",
            "ctrl-z",
            "ctrl-y",
            "ctrl-left",
            "ctrl-right",
            "ctrl-.",
            "ctrl-f",
            "ctrl-h",
        ]
        .map(|key| KeyBinding::new(key, NoAction, INPUT)),
    );
    cx.bind_keys([
        KeyBinding::new("cmd-a", SelectAll, INPUT),
        KeyBinding::new("cmd-c", Copy, INPUT),
        KeyBinding::new("cmd-x", Cut, INPUT),
        // The web backend reads clipboard data from the DOM paste event.
        // Its synchronous clipboard read returns None, so a Paste action
        // would swallow Command-V without inserting anything.
        KeyBinding::new("cmd-v", NoAction, INPUT),
        KeyBinding::new("cmd-z", Undo, INPUT),
        KeyBinding::new("cmd-shift-z", Redo, INPUT),
        KeyBinding::new("ctrl-a", MoveHome, INPUT),
        KeyBinding::new("ctrl-e", MoveEnd, INPUT),
        KeyBinding::new("cmd-left", MoveHome, INPUT),
        KeyBinding::new("cmd-right", MoveEnd, INPUT),
        KeyBinding::new("cmd-up", MoveToStart, INPUT),
        KeyBinding::new("cmd-down", MoveToEnd, INPUT),
        KeyBinding::new("alt-left", MoveToPreviousWord, INPUT),
        KeyBinding::new("alt-right", MoveToNextWord, INPUT),
        KeyBinding::new("ctrl-shift-a", SelectToStartOfLine, INPUT),
        KeyBinding::new("ctrl-shift-e", SelectToEndOfLine, INPUT),
        KeyBinding::new("cmd-shift-left", SelectToStartOfLine, INPUT),
        KeyBinding::new("cmd-shift-right", SelectToEndOfLine, INPUT),
        KeyBinding::new("cmd-shift-up", SelectToStart, INPUT),
        KeyBinding::new("cmd-shift-down", SelectToEnd, INPUT),
        KeyBinding::new("alt-shift-left", SelectToPreviousWordStart, INPUT),
        KeyBinding::new("alt-shift-right", SelectToNextWordEnd, INPUT),
        KeyBinding::new("ctrl-backspace", Backspace, INPUT),
        KeyBinding::new("cmd-backspace", DeleteToBeginningOfLine, INPUT),
        KeyBinding::new("cmd-delete", DeleteToEndOfLine, INPUT),
        KeyBinding::new("alt-backspace", DeleteToPreviousWordStart, INPUT),
        KeyBinding::new("alt-delete", DeleteToNextWordEnd, INPUT),
        KeyBinding::new(
            "cmd-enter",
            Enter {
                secondary: true,
                shift: false,
            },
            INPUT,
        ),
        KeyBinding::new("cmd-]", Indent, INPUT),
        KeyBinding::new("cmd-[", Outdent, INPUT),
        KeyBinding::new("cmd-f", Search, INPUT),
        KeyBinding::new("cmd-shift-f", Replace, INPUT),
        KeyBinding::new("cmd-.", ToggleCodeActions, INPUT),
    ]);
}
