use std::collections::HashMap;

use gpui::{App, Entity, Global, KeyBinding, WeakEntity, Window, WindowId};

use super::TABLE_KEY_CONTEXT;
use super::startup::WalletStartupRoot;

#[cfg(feature = "hardware")]
pub(super) const TREZOR_PASSPHRASE_MODE_KEY_CONTEXT: &str = "TrezorPassphraseMode";

#[cfg(feature = "hardware")]
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
pub(super) struct CycleTrezorPassphraseMode;

#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
pub(crate) struct UtxoPageUp;

#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
pub(crate) struct UtxoPageDown;

#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
pub(crate) struct UtxoHome;

#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
pub(crate) struct UtxoEnd;

#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
pub(crate) struct OpenSettings;

#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
pub(crate) struct LockVault;

#[derive(Default)]
struct WalletShortcutRegistry {
    roots_by_window: HashMap<WindowId, WeakEntity<WalletStartupRoot>>,
}

impl Global for WalletShortcutRegistry {}

#[derive(Clone, Copy)]
enum WalletShortcutAction {
    OpenSettings,
    LockVault,
}

pub(crate) fn install_wallet_action_bindings(app: &mut App) {
    app.bind_keys([
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-l", LockVault, None),
    ]);
    app.on_action(|_: &OpenSettings, cx| {
        dispatch_wallet_shortcut(WalletShortcutAction::OpenSettings, cx);
    });
    app.on_action(|_: &LockVault, cx| {
        dispatch_wallet_shortcut(WalletShortcutAction::LockVault, cx);
    });
    #[cfg(feature = "hardware")]
    app.bind_keys([KeyBinding::new(
        "tab",
        CycleTrezorPassphraseMode,
        Some(TREZOR_PASSPHRASE_MODE_KEY_CONTEXT),
    )]);
}

pub(super) fn register_wallet_shortcut_root(
    window: &Window,
    root: &Entity<WalletStartupRoot>,
    cx: &mut App,
) {
    let window_id = window.window_handle().window_id();
    cx.default_global::<WalletShortcutRegistry>()
        .roots_by_window
        .insert(window_id, root.downgrade());
}

fn dispatch_wallet_shortcut(action: WalletShortcutAction, cx: &mut App) {
    let Some(window_handle) = cx.active_window() else {
        return;
    };
    let window_id = window_handle.window_id();
    let Some(root) = cx
        .try_global::<WalletShortcutRegistry>()
        .and_then(|registry| registry.roots_by_window.get(&window_id))
        .and_then(WeakEntity::upgrade)
    else {
        return;
    };

    cx.defer(move |cx| {
        let _ = window_handle.update(cx, |_, window, cx| {
            root.update(cx, |root, cx| match action {
                WalletShortcutAction::OpenSettings => root.open_settings_from_shortcut(window, cx),
                WalletShortcutAction::LockVault => root.lock_vault_from_shortcut(window, cx),
            });
        });
    });
}

pub(crate) fn install_utxo_navigation_bindings(app: &mut App) {
    app.bind_keys([
        KeyBinding::new("pageup", UtxoPageUp, Some(TABLE_KEY_CONTEXT)),
        KeyBinding::new("pagedown", UtxoPageDown, Some(TABLE_KEY_CONTEXT)),
        KeyBinding::new("home", UtxoHome, Some(TABLE_KEY_CONTEXT)),
        KeyBinding::new("end", UtxoEnd, Some(TABLE_KEY_CONTEXT)),
    ]);
}

#[cfg(all(test, feature = "hardware"))]
mod tests {
    use gpui::{
        AppContext as _, Context, FocusHandle, InteractiveElement as _, IntoElement, Keystroke,
        ParentElement as _, Render, TestAppContext, Window, WindowOptions, div,
    };

    use super::*;

    const PROBE_INPUT_CONTEXT: &str = "TrezorPassphraseInputProbe";
    const PROBE_ROOT_CONTEXT: &str = "TrezorPassphraseRootProbe";

    #[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
    #[action(no_json)]
    struct ProbeInputTab;

    #[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
    #[action(no_json)]
    struct ProbeRootTab;

    struct TrezorPassphraseTabProbe {
        input_focus: FocusHandle,
        cycle_count: usize,
        root_tab_count: usize,
    }

    impl Render for TrezorPassphraseTabProbe {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            div()
                .key_context(PROBE_ROOT_CONTEXT)
                .on_action(cx.listener(|probe, _: &ProbeRootTab, _, _| {
                    probe.root_tab_count += 1;
                }))
                .child(
                    div()
                        .key_context(TREZOR_PASSPHRASE_MODE_KEY_CONTEXT)
                        .on_action(cx.listener(|probe, _: &CycleTrezorPassphraseMode, _, _| {
                            probe.cycle_count += 1;
                        }))
                        .child(
                            div()
                                .key_context(PROBE_INPUT_CONTEXT)
                                .track_focus(&self.input_focus),
                        ),
                )
        }
    }

    #[gpui::test]
    fn trezor_passphrase_tab_action_precedes_input_and_root_tab_actions(cx: &mut TestAppContext) {
        cx.update(|app| {
            app.bind_keys([
                KeyBinding::new("tab", ProbeRootTab, Some(PROBE_ROOT_CONTEXT)),
                KeyBinding::new("tab", ProbeInputTab, Some(PROBE_INPUT_CONTEXT)),
            ]);
            install_wallet_action_bindings(app);
        });
        let window = cx.update(|app| {
            app.open_window(WindowOptions::default(), |_, cx| {
                cx.new(|cx| TrezorPassphraseTabProbe {
                    input_focus: cx.focus_handle(),
                    cycle_count: 0,
                    root_tab_count: 0,
                })
            })
            .expect("open probe window")
        });
        window
            .update(cx, |probe, window, cx| {
                let input_focus = probe.input_focus.clone();
                cx.defer_in(window, move |_probe, window, _cx| {
                    input_focus.focus(window);
                });
            })
            .expect("schedule probe input focus");
        cx.run_until_parked();
        window
            .update(cx, |probe, window, _cx| {
                assert!(probe.input_focus.is_focused(window));
            })
            .expect("verify deferred probe focus");

        cx.dispatch_keystroke(
            *window,
            Keystroke::parse("tab").expect("valid Tab keystroke"),
        );

        window
            .update(cx, |probe, _, _| {
                assert_eq!(probe.cycle_count, 1);
                assert_eq!(probe.root_tab_count, 0);
            })
            .expect("inspect probe state");
    }
}
