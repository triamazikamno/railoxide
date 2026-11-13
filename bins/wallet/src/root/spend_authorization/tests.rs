use super::*;
use std::time::Instant;
use ui::controls::{app_button, app_masked_input};

use alloy::primitives::U256;
use gpui::{InteractiveElement, IntoElement};
use gpui_component::{button::ButtonVariants, input::InputState, select::SelectEvent};

use super::super::device_auth::{DeviceAuthMethod, DeviceAuthPassword};
use super::super::new_masked_input;
use super::summary_view::{
    new_spend_authorization_lifetime_select, render_spend_authorization_lifetime_row,
};

struct DialogWindow;

impl gpui::Render for DialogWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full()
    }
}

#[gpui::test]
fn spend_device_auth_is_inside_the_password_field_and_activates_from_the_keyboard(
    cx: &mut gpui::TestAppContext,
) {
    use gpui_kit::test::TestWindowExt as _;
    use wallet_ops::vault::DeviceAuthStatus;

    let directory = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.executor().allow_parking();
    cx.update(gpui_component::init);
    cx.update(crate::root::install_wallet_action_bindings);
    let mut root = None;
    let (_host, cx) = cx.add_window_view(|window, cx| {
        root = Some(crate::root::tests::public_accounts::fixture_root(
            directory.path(),
            &runtime,
            window,
            cx,
        ));
        let view = cx.new(|_| DialogWindow);
        gpui_component::Root::new(view, window, cx)
    });
    let root = root.unwrap();
    cx.simulate_resize(gpui::size(px(1000.), px(800.)));

    for (method, key, button_id) in [
        (
            DeviceAuthMethod::TouchId,
            "enter",
            "wallet-spend-auth-touch-id",
        ),
        (
            DeviceAuthMethod::AppleWatch,
            "space",
            "wallet-spend-auth-apple-watch",
        ),
    ] {
        let dialog = cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.open_spend_authorization_dialog_with_review(
                    SpendAuthorizationIntent::WalletConnectRequest {
                        request_key: "removed-request".into(),
                        review_token: 0,
                        reviewed_fee: None,
                    },
                    SpendAuthorizationSummary::new("Review", "", Vec::new()),
                    None,
                    window,
                    cx,
                )
            })
        });
        assert_eq!(
            root.read_with(cx, |root, _| root.platform_attention_count),
            1,
            "an open review counts toward platform attention"
        );
        cx.update(|window, cx| {
            let prompt = root.update(cx, |root, _| {
                // Show the action without enrolling or opening a native device_auth prompt.
                root.touch_id_supported = true;
                root.apple_watch_supported = true;
                root.apple_watch_status = DeviceAuthStatus::Enabled;
                root.touch_id_status = DeviceAuthStatus::Enabled;
                root.device_auth_prompt_cached()
            });
            dialog.update(cx, |dialog, cx| {
                dialog.device_auth = prompt;
                dialog.password_input.update(cx, |input, cx| {
                    input.set_value("unsubmitted password", window, cx);
                });
                dialog.focus_password(window, cx);
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            let input_id = dialog.read(cx).password_input.entity_id();
            let group_id = ("vault-password-device-auth", input_id);
            let group = window.find(group_id).bounds();
            let button = window.within(group_id).find(button_id);
            let bounds = button.bounds();
            assert!(
                group.left() <= bounds.left()
                    && group.top() <= bounds.top()
                    && bounds.right() <= group.right()
                    && bounds.bottom() <= group.bottom()
            );
            assert_eq!(
                bounds.size.width, bounds.size.height,
                "the action is icon-only"
            );
            let fingerprint = window
                .within(group_id)
                .find("wallet-spend-auth-touch-id")
                .bounds();
            let watch = window
                .within(group_id)
                .find("wallet-spend-auth-apple-watch")
                .bounds();
            assert!(
                fingerprint.right() <= watch.left(),
                "authentication buttons overlap"
            );
            window.press("tab", cx);
            if method == DeviceAuthMethod::AppleWatch {
                window.press("tab", cx);
            }
            assert_eq!(window.find(button_id).focused(), Some(true));
        });
        let keystroke = gpui::Keystroke::parse(key).unwrap();
        cx.simulate_event(gpui::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(gpui::KeyUpEvent { keystroke });
        cx.run_until_parked();
        let deadline = Instant::now() + Duration::from_secs(5);
        while dialog.read_with(cx, |dialog, _| dialog.device_auth_pending) {
            assert!(
                Instant::now() < deadline,
                "device authentication did not finish"
            );
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            let dialog = dialog.read(cx);
            // The fixture has no sealed password, so activation takes the failure path.
            assert!(
                dialog
                    .device_auth
                    .as_ref()
                    .is_some_and(|prompt| !prompt.includes(method)
                        && prompt.includes(match method {
                            DeviceAuthMethod::TouchId => DeviceAuthMethod::AppleWatch,
                            DeviceAuthMethod::AppleWatch => DeviceAuthMethod::TouchId,
                        })),
                "the selected method must fail without removing the other method"
            );
            assert!(dialog.error.is_some());
            assert_eq!(
                dialog.password_input.read(cx).value(),
                "unsubmitted password"
            );
            assert!(!dialog.pending);
            assert!(root.read(cx).spend_authorization_cache.is_none());
            assert!(window.has_active_dialog(cx));
            window.close_all_dialogs(cx);
            root.update(cx, |root, _| {
                root.sync_platform_attention_for_window(window);
                assert_eq!(
                    root.platform_attention_count, 0,
                    "a closed review releases its platform attention"
                );
            });
        });
    }
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn spend_device_auth_requires_open_review_and_watch_never_caches_approval(
    cx: &mut gpui::TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.executor().allow_parking();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_host, cx) = cx.add_window_view(|window, cx| {
        root = Some(crate::root::tests::public_accounts::fixture_root(
            directory.path(),
            &runtime,
            window,
            cx,
        ));
        let view = cx.new(|_| DialogWindow);
        gpui_component::Root::new(view, window, cx)
    });
    let root = root.unwrap();
    cx.simulate_resize(gpui::size(px(1000.), px(800.)));
    let open_review = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.spend_authorization_lifetime = SpendAuthorizationLifetime::FiveMinutes;
                root.open_spend_authorization_dialog_with_review(
                    // The removed request prevents transaction submission while
                    // authorization caching still exercises the real completion.
                    SpendAuthorizationIntent::WalletConnectRequest {
                        request_key: "removed-request".into(),
                        review_token: 0,
                        reviewed_fee: None,
                    },
                    SpendAuthorizationSummary::new("Review", "", Vec::new()),
                    None,
                    window,
                    cx,
                )
            })
        })
    };
    let complete = |dialog: &Entity<SpendAuthorizationDialogContent>,
                    method: DeviceAuthMethod,
                    window: &mut Window,
                    cx: &mut gpui::App| {
        dialog.update(cx, |dialog, cx| {
            dialog.finish_device_auth(
                method,
                DeviceAuthPassword::Password(Zeroizing::new("public list test password".into())),
                window,
                cx,
            );
        });
    };
    let wait_for_password = |dialog: &Entity<SpendAuthorizationDialogContent>,
                             cx: &mut gpui::VisualTestContext| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while dialog.read_with(cx, |dialog, _| dialog.pending) {
            assert!(Instant::now() < deadline, "password check did not finish");
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
            cx.run_until_parked();
        }
    };
    for completion in ["device_auth", "password_check"] {
        // Retain the entity as rendered button callbacks can do until redraw.
        let dialog = open_review(cx);
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| {
            dialog.update(cx, |dialog, _| dialog.device_auth_pending = true);
            if completion == "password_check" {
                complete(&dialog, DeviceAuthMethod::TouchId, window, cx);
            }
            root.update(cx, |root, cx| root.open_settings_from_shortcut(window, cx));
            if completion == "device_auth" {
                complete(&dialog, DeviceAuthMethod::TouchId, window, cx);
            }
        });
        wait_for_password(&dialog, cx);
        assert!(
            root.read_with(cx, |root, _| root.spend_authorization_cache.is_none()),
            "dismissed review populated the authorization cache"
        );
        cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
    }
    // Both completions pass the real password check. The selected five-minute
    // lifetime applies to Touch ID, but Watch must leave no reusable approval,
    // including when a previous Touch ID approval is still cached.
    for method in DeviceAuthMethod::ALL {
        let dialog = open_review(cx);
        cx.update(|window, cx| complete(&dialog, method, window, cx));
        wait_for_password(&dialog, cx);
        assert!(dialog.read_with(cx, |dialog, _| dialog.error.is_none()));
        assert_eq!(
            root.update(cx, |root, cx| {
                root.valid_spend_authorization_password(cx).is_some()
            }),
            method == DeviceAuthMethod::TouchId
        );
    }
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn password_check_refreshes_device_auth_without_authorizing_stale_requests(
    cx: &mut gpui::TestAppContext,
) {
    // Password verification wakes GPUI from Tokio's blocking pool.
    cx.executor().allow_parking();
    let path = std::env::temp_dir().join(format!(
        "spend-auth-ui-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let wallet = crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let current_touch_id_status = root.read_with(cx, |root, _| {
        root.vault_store
            .as_ref()
            .unwrap()
            .device_auth_status(wallet_ops::device_auth::DeviceAuthMethod::TouchId)
            .unwrap()
    });
    for interruption in ["cancel", "wallet", "drop"] {
        let lease = Rc::new(Cell::new(true));
        let dialog = cx.update(|window, cx| {
            cx.new(|cx| {
                SpendAuthorizationDialogContent::new(
                    root.clone(),
                    // A removed request cannot submit a transaction. The remembered
                    // authorization would still be populated if the stale check ran.
                    SpendAuthorizationIntent::WalletConnectRequest {
                        request_key: "removed-request".into(),
                        review_token: 0,
                        reviewed_fee: None,
                    },
                    SpendAuthorizationSummary::new("Review", "", Vec::new()),
                    SpendAuthorizationLifetime::FiveMinutes,
                    Rc::downgrade(&lease),
                    window,
                    cx,
                )
            })
        });
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                root.touch_id_status = wallet_ops::vault::DeviceAuthStatus::NeedsReenrollment;
            });
            dialog.update(cx, |dialog, cx| {
                dialog.password_input.update(cx, |input, cx| {
                    input.set_value("public list test password", window, cx);
                });
                dialog.submit(window, cx);
                assert!(dialog.pending);
                if interruption == "wallet" {
                    root.update(cx, |root, _| root.advance_active_wallet_generation());
                } else if interruption == "cancel" {
                    dialog.cancel(cx);
                }
            });
        });
        let old_dialog = dialog.downgrade();
        let dialog = (interruption != "drop").then_some(dialog);
        let deadline = Instant::now() + Duration::from_secs(5);
        while dialog.as_ref().map_or_else(
            || {
                root.read_with(cx, |root, _| {
                    root.touch_id_status != current_touch_id_status
                })
            },
            |dialog| dialog.read_with(cx, |dialog, _| dialog.pending),
        ) {
            assert!(Instant::now() < deadline, "password check did not finish");
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
            cx.run_until_parked();
        }
        assert_eq!(
            root.read_with(cx, |root, _| root.touch_id_status),
            current_touch_id_status,
            "password verification did not refresh persisted device_auth status"
        );
        assert!(root.read_with(cx, |root, _| root.spend_authorization_cache.is_none()));
        if interruption == "drop" {
            assert!(old_dialog.upgrade().is_none());
        }
        if interruption == "wallet" {
            assert!(
                dialog
                    .unwrap()
                    .read_with(cx, |dialog, _| dialog.error.is_some())
            );
        }
    }
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    drop(entered);
    drop(runtime);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn executor_quote_deltas_preserve_small_changes_and_distinguish_adverse_direction() {
    let approved = U256::from(100);
    for (current, higher_is_worse, sign, adverse) in [
        (101, true, "+", true),
        (99, true, "−", false),
        (101, false, "+", false),
        (99, false, "−", true),
    ] {
        let row = SpendAuthorizationSummaryRow::new("", "").with_amount_change(
            Some(approved),
            U256::from(current),
            higher_is_worse,
            |amount| crate::root::format_unshield_amount_input(amount, Some(18)),
        );
        let delta = row.delta.expect("one wei change remains visible");
        assert_eq!(delta.text, format!("{sign}0.000000000000000001"));
        assert_eq!(delta.adverse, adverse);
    }
    for previous in [None, Some(approved)] {
        let row = SpendAuthorizationSummaryRow::new("", "").with_amount_change(
            previous,
            approved,
            true,
            |_| panic!("initial and unchanged amounts have no delta"),
        );
        assert!(row.delta.is_none());
    }
}

/// A change is told in token units and in USD, in USD alone for amounts in different
/// tokens, and in token units alone without a price.
#[test]
fn priced_amount_deltas_show_the_token_change_the_usd_change_or_both() {
    let amounts = Some((U256::from(400_000), U256::from(520_000)));
    let values = Some((U256::from(400_000), U256::from(520_000)));
    // A cheaper limit in another token.
    let lower = Some((U256::from(520_000), U256::from(440_000)));
    for (amounts, values, text, adverse) in [
        (amounts, values, "+0.12 USDC · +$0.12", true),
        (None, lower, "−$0.08", false),
        (amounts, None, "+0.12 USDC", true),
    ] {
        let row = SpendAuthorizationSummaryRow::new("", "").with_priced_amount_change(
            amounts,
            values,
            true,
            |amount| {
                format!(
                    "{} USDC",
                    crate::root::format_unshield_amount_input(amount, Some(6))
                )
            },
            railgun_ui::format_usd_micro_value,
        );
        let delta = row.delta.expect("a changed amount");
        assert_eq!((delta.text.as_str(), delta.adverse), (text, adverse));
    }
    // No price for limits in different tokens leaves nothing to compare.
    let row = SpendAuthorizationSummaryRow::new("", "").with_priced_amount_change(
        None,
        None,
        true,
        |_| unreachable!(),
        |_| unreachable!(),
    );
    assert!(row.delta.is_none());
}

struct LifetimePickerProbe {
    focus: gpui::FocusHandle,
    password_input: Entity<InputState>,
    lifetime_select: Entity<SpendAuthorizationLifetimeSelect>,
    width: gpui::Pixels,
}

impl gpui::Render for LifetimePickerProbe {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        gpui_kit::base::Dialog::new(cx)
            .focus_handle(self.focus.clone())
            .on_ok(|_, _, _| false)
            .popup(
                div()
                    .w(self.width)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .debug_selector(|| "lifetime-content".to_owned())
                    .child(
                        div()
                            .debug_selector(|| "lifetime-password".to_owned())
                            .child(app_masked_input(&self.password_input, false)),
                    )
                    .child(
                        div()
                            .w_full()
                            .debug_selector(|| "lifetime-row".to_owned())
                            .child(render_spend_authorization_lifetime_row(
                                &self.lifetime_select,
                                false,
                            )),
                    )
                    .child(
                        div()
                            .debug_selector(|| "lifetime-footer".to_owned())
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .justify_end()
                            .gap_2()
                            .child(app_button("lifetime-cancel", "Cancel").flex_none())
                            .child(
                                app_button("lifetime-submit", "Authorize and continue")
                                    .primary()
                                    .flex_none(),
                            ),
                    ),
            )
    }
}

#[gpui::test]
fn lifetime_select_stays_between_password_and_footer(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(ui::theme::apply_zenburn_component_theme);
    let (probe, cx) = cx.add_window_view(|window, cx| LifetimePickerProbe {
        focus: cx.focus_handle(),
        password_input: new_masked_input(window, cx, "Vault password"),
        lifetime_select: new_spend_authorization_lifetime_select(
            SpendAuthorizationLifetime::UntilVaultLock,
            window,
            cx,
        ),
        width: px(400.0),
    });
    for width in [400.0, 280.0] {
        probe.update(cx, |probe, cx| {
            probe.width = px(width);
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let content = cx.debug_bounds("lifetime-content").expect("dialog content");
        let password = cx
            .debug_bounds("lifetime-password")
            .expect("password input");
        let row = cx.debug_bounds("lifetime-row").expect("lifetime row");
        let select = cx
            .debug_bounds("wallet-spend-auth-lifetime-select")
            .expect("lifetime select");
        let footer = cx.debug_bounds("lifetime-footer").expect("dialog footer");
        assert!(
            password.bottom() <= row.top(),
            "row overlaps password at {width}"
        );
        assert!(
            row.bottom() <= footer.top(),
            "row overlaps footer at {width}"
        );
        assert!(select.size.height > px(0.0), "select collapsed at {width}");
        assert!(
            select.top() >= row.top() && select.bottom() <= row.bottom(),
            "row does not contain the select at {width}"
        );
        assert!(
            select.left() >= content.left() && select.right() <= content.right(),
            "select overflows the dialog at {width}"
        );
    }
}

#[gpui::test]
fn choosing_a_lifetime_in_the_select_updates_the_dialog(cx: &mut gpui::TestAppContext) {
    let path = std::env::temp_dir().join(format!(
        "spend-auth-lifetime-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let wallet = crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let lease = Rc::new(Cell::new(true));
    let dialog = cx.update(|window, cx| {
        cx.new(|cx| {
            SpendAuthorizationDialogContent::new(
                root.clone(),
                SpendAuthorizationIntent::WalletConnectRequest {
                    request_key: "lifetime-request".into(),
                    review_token: 0,
                    reviewed_fee: None,
                },
                SpendAuthorizationSummary::new("Review", "", Vec::new()),
                SpendAuthorizationLifetime::Once,
                Rc::downgrade(&lease),
                window,
                cx,
            )
        })
    });
    let select = dialog.read_with(cx, |dialog, _| dialog.lifetime_select.clone());
    select.update(cx, |_, cx| {
        cx.emit(
            SelectEvent::<SearchableVec<SpendAuthorizationLifetime>>::Confirm(Some(
                SpendAuthorizationLifetime::FifteenMinutes,
            )),
        );
    });
    cx.run_until_parked();
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.lifetime),
        SpendAuthorizationLifetime::FifteenMinutes
    );
    cx.update(|window, _| window.remove_window());
    drop(dialog);
    drop(host);
    drop(root);
    cx.run_until_parked();
    drop(entered);
    drop(runtime);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn timed_authorization_expires_when_only_wall_clock_advances() {
    let scope = SpendAuthorizationScope::new("base", "wallet", None);
    let granted = AutoLockTimestamp::now();
    for lifetime in [
        SpendAuthorizationLifetime::FiveMinutes,
        SpendAuthorizationLifetime::FifteenMinutes,
    ] {
        let cache = SpendAuthorizationCache::new(
            Zeroizing::new("password".to_owned()),
            lifetime,
            scope.clone(),
            granted,
        )
        .expect("timed cache");
        let resumed = granted.with_wall_advanced(lifetime.duration().expect("timed"));

        assert!(!cache.is_valid_at(&scope, resumed));
    }
}

#[test]
fn remembered_authorization_is_bound_to_exact_wallet_scope() {
    let now = AutoLockTimestamp::now();
    let first_scope = SpendAuthorizationScope::new("base-a", "wallet-a", None);
    let second_scope = SpendAuthorizationScope::new("base-a", "wallet-b", None);
    let session_scope = SpendAuthorizationScope::new(
        "base-a",
        "wallet-a",
        Some(SoftwareSeedSessionBinding::new(
            "base-a",
            "wallet-a",
            wallet_ops::vault::VaultSessionId::from_bytes([7; 16]),
        )),
    );
    let cache = SpendAuthorizationCache::new(
        Zeroizing::new("password".to_owned()),
        SpendAuthorizationLifetime::UntilVaultLock,
        first_scope.clone(),
        now,
    )
    .expect("remembered cache");

    assert!(cache.is_valid_at(&first_scope, now));
    assert!(!cache.is_valid_at(&second_scope, now));
    assert!(!cache.is_valid_at(&session_scope, now));
}

#[test]
fn context_cleanup_drops_protected_seed_and_remembered_spend_authorization() {
    let created = wallet_ops::vault::create_with_params(
        "test-vault-password",
        wallet_ops::vault::KdfParams::default(),
    )
    .expect("create test vault");
    let binding = SoftwareSeedSessionBinding::new(
        "base-profile",
        "child-context",
        wallet_ops::vault::VaultSessionId::from_bytes([8; 16]),
    );
    let protected = created
        .spend
        .seal_software_seed_session(binding.clone(), &[7; 64])
        .expect("seal protected seed");
    let scope = SpendAuthorizationScope::new("base-profile", "child-context", Some(binding));
    let mut protected = Some(Arc::new(protected));
    let mut remembered = SpendAuthorizationCache::new(
        Zeroizing::new("test-vault-password".to_owned()),
        SpendAuthorizationLifetime::UntilVaultLock,
        scope,
        AutoLockTimestamp::now(),
    );

    assert!(clear_protected_software_seed_session_state(
        &mut protected,
        &mut remembered,
    ));
    assert!(protected.is_none());
    assert!(remembered.is_none());
}
