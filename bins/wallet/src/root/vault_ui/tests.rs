use super::*;
use gpui::{
    AppContext as _, Context, KeyDownEvent, KeyUpEvent, Keystroke, Render, TestAppContext,
    VisualTestContext,
};
use gpui_component::Root;
use gpui_kit::test::TestWindowExt as _;
use wallet_ops::vault::DeviceAuthStatus;

struct UnlockWindow(Entity<WalletRoot>);

impl Render for UnlockWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .w_full()
            .p_4()
            .child(self.0.read(cx).render_unlock_vault(self.0.clone()))
    }
}

fn press_key(cx: &mut VisualTestContext, key: &str) {
    let keystroke = Keystroke::parse(key).unwrap();
    cx.simulate_event(KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    // Native button activation completes on key release.
    cx.simulate_event(KeyUpEvent { keystroke });
}

#[gpui::test]
fn unlock_device_auth_is_inside_the_password_field_and_activates_from_the_keyboard(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    cx.update(crate::root::install_wallet_action_bindings);
    let mut root = None;
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let wallet = crate::root::tests::public_accounts::fixture_root(
            directory.path(),
            &runtime,
            window,
            cx,
        );
        root = Some(wallet.clone());
        let view = cx.new(|cx| {
            cx.observe(&wallet, |_, _, cx| cx.notify()).detach();
            UnlockWindow(wallet)
        });
        Root::new(view, window, cx)
    });
    let root = root.unwrap();
    cx.simulate_resize(gpui::size(px(480.), px(320.)));

    for key in ["space", "enter"] {
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.lock_vault(window, cx);
                // Display the macOS action without requiring a device_auth record in the fixture.
                root.touch_id_supported = true;
                root.touch_id_status = DeviceAuthStatus::Enabled;
                root.unlock_password_input.update(cx, |input, cx| {
                    input.set_value("unsubmitted password", window, cx);
                    input.focus(window, cx);
                });
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            let input_id = root.read(cx).unlock_password_input.entity_id();
            let input = window.find(("input", input_id));
            assert_eq!(input.role(), Some(gpui::accesskit::Role::PasswordInput));
            assert!(
                input.value().is_none(),
                "the password must stay out of accessibility values"
            );
            let group_id = ("vault-password-device-auth", input_id);
            let group = window.find(group_id).bounds();
            let button = window.within(group_id).find("unlock-wallet-vault-touch-id");
            let bounds = button.bounds();
            assert!(
                group.left() <= bounds.left()
                    && group.top() <= bounds.top()
                    && bounds.right() <= group.right()
                    && bounds.bottom() <= group.bottom(),
                "Touch ID must render inside the password field"
            );
            assert!(
                button.label().is_some(),
                "the icon needs an accessible name"
            );
            window.press("tab", cx);
            assert_eq!(
                window.find("unlock-wallet-vault-touch-id").focused(),
                Some(true),
                "Tab from the password must reach Touch ID before password submission"
            );
        });
        press_key(cx, key);
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            // Activation rechecks enrollment. The fixture has none, so the icon disappears.
            assert!(window.try_find("unlock-wallet-vault-touch-id").is_none());
            let root = root.read(cx);
            assert_eq!(
                root.unlock_password_input.read(cx).value(),
                "unsubmitted password"
            );
            assert!(matches!(root.vault_state, VaultState::UnlockVault));
            assert!(!root.unlock_in_progress);
        });
    }

    for (password_pending, device_auth_pending) in [(true, false), (false, true)] {
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.touch_id_supported = true;
                root.touch_id_status = DeviceAuthStatus::Enabled;
                root.unlock_in_progress = password_pending;
                root.device_auth_in_progress = device_auth_pending;
                root.unlock_password_input.update(cx, |input, cx| {
                    input.focus(window, cx);
                });
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            let input_id = root.read(cx).unlock_password_input.entity_id();
            window.click(("input", input_id), cx);
            window.input("blocked", cx);
            window.press("tab", cx);
            assert_ne!(
                window.find("unlock-wallet-vault-touch-id").focused(),
                Some(true),
                "pending unlocks must keep Touch ID out of the tab order"
            );
            window.click("unlock-wallet-vault-touch-id", cx);
        });
        press_key(cx, "space");
        press_key(cx, "enter");
        cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("unlock-wallet-vault-touch-id").is_some());
            assert_eq!(
                root.read(cx).unlock_password_input.read(cx).value(),
                "unsubmitted password"
            );
        });
    }
    cx.update(|window, _| window.remove_window());
}
