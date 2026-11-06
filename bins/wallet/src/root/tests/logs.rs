use super::*;
use crate::root::actions::*;
use crate::root::logs_search::hide;
use gpui::{Entity, Modifiers, ParentElement as _, TestAppContext, VisualTestContext, point, px};
use gpui_component::Root;
use gpui_component::input::{Input, InputState};
use ui::dialog::WalletDialogExt as _;
use ui::logs::{LogEntry, LogStore, LogsPane, format_log_lines};

const SECONDARY: &str = if cfg!(target_os = "macos") {
    "cmd"
} else {
    "ctrl"
};

/// A wallet window with the logs panel open on five lines, one per level, oldest first.
fn open_logs<'a>(
    cx: &'a mut TestAppContext,
    path: &std::path::Path,
    runtime: &tokio::runtime::Runtime,
) -> (
    Entity<Root>,
    Entity<WalletRoot>,
    Vec<LogEntry>,
    &'a mut VisualTestContext,
) {
    cx.update(|cx| {
        gpui_component::init(cx);
        ui::theme::apply_zenburn_component_theme(cx);
        install_wallet_action_bindings(cx);
    });
    let mut root = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let wallet = super::public_accounts::fixture_root(path, runtime, window, cx);
        root = Some(wallet.clone());
        Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    cx.simulate_resize(gpui::size(px(1400.), px(1000.)));
    let store = LogStore::new(10);
    let levels = [
        tracing::Level::ERROR,
        tracing::Level::WARN,
        tracing::Level::INFO,
        tracing::Level::DEBUG,
        tracing::Level::TRACE,
    ];
    for (ix, level) in (0_u64..).zip(levels) {
        store.push(LogEntry {
            seq: 0,
            unix_ms: ix * 1_000,
            level,
            target: "wallet_ops::poi".into(),
            message: format!("line {ix}").into(),
        });
    }
    let lines = store.logs();
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            // The pane reads the store when made; the fixture's own holds no lines.
            root.logs = cx.new(|cx| LogsPane::new(store, window, cx));
            root.logs_open = true;
            cx.notify();
        });
        window.draw(cx).clear(cx);
    });
    (host, root, lines, cx)
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

fn close(host: Entity<Root>, root: Entity<WalletRoot>, cx: &mut VisualTestContext) {
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
}

#[gpui::test]
fn logs_keyboard_selects_and_copies_shown_lines(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("logs-keyboard");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let (host, root, lines, cx) = open_logs(cx, &path, &runtime);
    let pane = cx.update(|_, cx| root.read(cx).logs.clone());
    cx.update(|window, cx| pane.read(cx).focus_handle().focus(window, cx));
    draw(cx);
    let selected = |cx: &mut VisualTestContext| cx.update(|_, cx| pane.read(cx).selected_count());
    let copied = |cx: &mut VisualTestContext| cx.read_from_clipboard().and_then(|item| item.text());

    // Down starts at the newest line; Shift+Up extends to the one before it.
    cx.simulate_keystrokes("down shift-up");
    assert_eq!(selected(cx), 2);
    cx.simulate_keystrokes(&format!("{SECONDARY}-c"));
    assert_eq!(copied(cx), Some(format_log_lines(&lines[3..])));

    cx.simulate_keystrokes(&format!("{SECONDARY}-a {SECONDARY}-c"));
    assert_eq!(selected(cx), 5);
    assert_eq!(copied(cx), Some(format_log_lines(&lines)));

    cx.simulate_keystrokes("escape");
    assert_eq!(selected(cx), 0);

    close(host, root, cx);
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn logs_panel_holds_focus_above_a_dialog(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("logs-dialog-focus");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let (host, root, _, cx) = open_logs(cx, &path, &runtime);
    let input = cx.update(|window, cx| {
        let input = cx.new(|cx| InputState::new(window, cx).default_value("draft"));
        let dialog_input = input.clone();
        window.open_wallet_dialog(cx, move |dialog, _, _| {
            dialog.child(Input::new(&dialog_input))
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        input
    });
    draw(cx);
    let dialog_focused = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| input.read(cx).focus_handle(cx).is_focused(window))
    };
    let in_panel = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| root.read(cx).logs_panel_focus.contains_focused(window, cx))
    };
    let search_text = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| root.read(cx).logs_search.read(cx).text().to_owned())
    };
    let press = |cx: &mut VisualTestContext, position| {
        cx.simulate_click(position, Modifiers::default());
        draw(cx);
    };
    let escape = |cx: &mut VisualTestContext| {
        cx.simulate_keystrokes("escape");
        draw(cx);
    };
    let search = cx
        .debug_bounds("wallet-logs-search")
        .expect("logs search is rendered");
    let panel = cx
        .debug_bounds("wallet-logs-panel")
        .expect("logs panel is rendered");

    press(cx, search.center());
    cx.simulate_input("rpc");
    draw(cx);
    assert_eq!(search_text(cx), "rpc", "typing goes to the logs search");
    assert!(in_panel(cx) && !dialog_focused(cx));
    // Escape closes the suggestions, then clears the text, then gives focus back.
    escape(cx);
    escape(cx);
    assert_eq!(search_text(cx), "");
    assert!(
        in_panel(cx),
        "the panel keeps focus while it has something to dismiss"
    );
    escape(cx);
    assert!(
        dialog_focused(cx) && !in_panel(cx),
        "focus is back in the dialog"
    );
    cx.update(|_, cx| assert_eq!(input.read(cx).value(), "draft"));

    // A row press selects it; Escape clears the selection, then gives focus back.
    press(cx, point(panel.center().x, panel.bottom() - px(10.)));
    let pane = cx.update(|_, cx| root.read(cx).logs.clone());
    assert_eq!(cx.update(|_, cx| pane.read(cx).selected_count()), 1);
    escape(cx);
    assert!(in_panel(cx));
    assert_eq!(cx.update(|_, cx| pane.read(cx).selected_count()), 0);
    escape(cx);
    assert!(
        dialog_focused(cx),
        "focus is back in the dialog after the row"
    );

    // Tab and Shift+Tab cycle within the panel above the dialog.
    press(cx, search.center());
    for keys in ["tab", "shift-tab"] {
        for _ in 0..6 {
            cx.simulate_keystrokes(keys);
            draw(cx);
            assert!(
                in_panel(cx) && !dialog_focused(cx),
                "{keys} stays in the panel"
            );
        }
    }
    for _ in 0..3 {
        escape(cx);
        if !in_panel(cx) {
            break;
        }
    }
    assert!(dialog_focused(cx), "Escape unwinds to the dialog after Tab");

    // Without a dialog the panel is in the window's tab order. The network pill gives the
    // window a tab stop outside the panel.
    cx.update(gpui_component::WindowExt::close_all_dialogs);
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            root.network_health.mode = wallet_ops::WalletNetworkMode::Tor;
            root.network_health.state = wallet_ops::WalletNetworkHealthState::Ready;
            cx.notify();
        });
        window.draw(cx).clear(cx);
        let search = root.read(cx).logs_search.clone();
        search.update(cx, |search, cx| search.focus(window, cx));
    });
    draw(cx);
    assert!(in_panel(cx));
    let left_panel = (0..12).any(|_| {
        cx.simulate_keystrokes("tab");
        draw(cx);
        !in_panel(cx)
    });
    assert!(left_panel, "Tab leaves the panel without a dialog");

    close(host, root, cx);
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn logs_search_suggestions_open_above_the_field_without_room_below(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("logs-suggestions-above");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let (host, root, _, cx) = open_logs(cx, &path, &runtime);
    // The fixture's search reads its own one-line store; this one reads the pane's, with
    // enough modules to fill the suggestions.
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            let store = root.logs.read(cx).store().clone();
            for module in ["a", "b", "c", "d", "e"] {
                store.push(LogEntry {
                    seq: 0,
                    unix_ms: 0,
                    level: tracing::Level::INFO,
                    target: format!("wallet_ops::{module}").into(),
                    message: "line".into(),
                });
            }
            root.logs_search = cx.new(|cx| {
                crate::root::search_field::SearchField::new(
                    crate::root::logs_search::LogsSearchSource { store },
                    window,
                    cx,
                )
            });
            cx.notify();
        });
    });
    draw(cx);
    let search = cx
        .debug_bounds("wallet-logs-search")
        .expect("logs search is rendered");

    cx.simulate_click(search.center(), Modifiers::default());
    cx.simulate_input("wallet");
    draw(cx);
    draw(cx);
    let popup = cx
        .debug_bounds("wallet-search-suggestions")
        .expect("suggestions are open");
    let window_height = cx.update(|window, _| window.viewport_size().height);
    assert!(
        popup.size.height > window_height - search.bottom(),
        "the suggestions must not fit below the field for this test to mean anything"
    );
    assert!(!popup.is_empty());
    assert!(
        !popup.intersects(&search),
        "the suggestions {popup:?} cover the field {search:?}"
    );

    close(host, root, cx);
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn locking_the_vault_forgets_the_logs_search(cx: &mut TestAppContext) {
    let path = temp_wallet_db_root("logs-lock");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let (host, root, _, cx) = open_logs(cx, &path, &runtime);
    let search = cx.update(|_, cx| root.read(cx).logs_search.clone());
    cx.update(|window, cx| {
        let input = search.read(cx).input().clone();
        input.update(cx, |input, cx| input.set_value("rpc", window, cx));
        search.update(cx, |search, cx| {
            search.set_query("rpc", cx);
            search.search_text(cx);
            search.edit_chips(|chips| hide(chips, "arti_client"), cx);
        });
    });
    cx.update(|_, cx| assert_eq!(search.read(cx).recent().len(), 1));

    // Unlocking restores nothing of the search, so the state after locking is what it shows.
    cx.update(|window, cx| root.update(cx, |root, cx| root.lock_vault(window, cx)));
    cx.run_until_parked();
    cx.update(|_, cx| {
        let search = search.read(cx);
        assert_eq!(search.text(), "");
        assert_eq!(
            search.filter().chips,
            [] as [crate::root::logs_search::LogChip; 0]
        );
        assert_eq!(
            search.recent(),
            [] as [crate::root::search_field::RecentSearch<crate::root::logs_search::LogChip>; 0]
        );
        assert_eq!(search.input().read(cx).value(), "");
    });

    close(host, root, cx);
    std::fs::remove_dir_all(path).unwrap();
}
