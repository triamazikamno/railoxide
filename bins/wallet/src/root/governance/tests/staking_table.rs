use super::*;

#[gpui::test]
fn participant_picker_preserves_query_cursor_and_enrollment_order(cx: &TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx
        .update(|app| {
            app.open_window(WindowOptions::default(), |window, cx| {
                let accounts = vec![
                    account("alice", PublicAccountStatus::Active, false),
                    account("amy", PublicAccountStatus::Active, false),
                    account("ava", PublicAccountStatus::Active, false),
                    account("inactive", PublicAccountStatus::Inactive, false),
                    account("global", PublicAccountStatus::Active, true),
                ];
                let saved = vec!["inactive".to_owned(), "global".to_owned()];
                let selected = saved.iter().cloned().collect();
                let items = participant_choices(&accounts, &selected, Some("wallet"), "");
                let picker = cx.new(|cx| {
                    let mut picker =
                        ComboboxState::new(SearchableVec::new(items), vec![], window, cx)
                            .multiple(true)
                            .searchable(true);
                    picker.set_selected_values(&saved, window, cx);
                    picker
                });
                cx.new(|cx| {
                    cx.subscribe_in(
                        &picker,
                        window,
                        |this: &mut ParticipantPickerProbe, picker, event, window, cx| {
                            if let Some(values) = participant_picker_change(
                                event,
                                &this.accounts,
                                Some("wallet"),
                                Some("wallet"),
                                &this.saved,
                            ) {
                                this.saved = values.clone();
                                this.commits.push(values);
                                this.sync_picker(window, cx);
                                cx.notify();
                            }
                            if matches!(event, ComboboxEvent::Confirm(_)) {
                                picker.update(cx, |picker, cx| picker.set_query("", window, cx));
                            }
                        },
                    )
                    .detach();
                    ParticipantPickerProbe {
                        governance: GovernanceState::new(
                            picker,
                            cx.new(|cx| InputState::new(window, cx)),
                            cx.new(|cx| InputState::new(window, cx)),
                            Eip1559GasFeeEditorState::new(window, cx),
                        ),
                        accounts,
                        saved,
                        commits: Vec::new(),
                    }
                })
            })
        })
        .expect("open participant picker probe");
    let mut cx = VisualTestContext::from_window(*window, cx);
    cx.refresh().expect("render participant picker");
    let probe = window.root(&mut cx).expect("participant picker probe");
    let picker = probe.read_with(&cx, |probe, _| probe.governance.participant_picker.clone());
    cx.update(|window, cx| picker.read(cx).focus_handle(cx).focus(window, cx));
    cx.simulate_keystrokes("down");
    cx.update(|window, cx| {
        picker.update(cx, |picker, cx| picker.set_query(" alice ", window, cx));
    });
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    probe.read_with(&cx, |probe, _| {
        assert_eq!(probe.saved, ["inactive", "global", "alice"]);
        assert_eq!(
            probe.commits.len(),
            1,
            "Change must commit before dismissal"
        );
    });
    cx.update(|window, cx| picker.update(cx, |picker, cx| picker.set_query("a", window, cx)));
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    cx.refresh().expect("render after selecting Amy");
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    probe.read_with(&cx, |probe, cx| {
        assert_eq!(probe.saved, ["inactive", "global", "alice", "amy"]);
        assert_eq!(probe.governance.participant_picker.read(cx).query(cx), "a");
    });
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    cx.refresh().expect("render after selecting Ava");
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    probe.read_with(&cx, |probe, cx| {
        assert_eq!(probe.saved, ["inactive", "global", "alice", "amy", "ava"]);
        assert_eq!(probe.governance.participant_picker.read(cx).query(cx), "a");
    });
    cx.update(|window, cx| {
        picker.update(cx, |picker, cx| picker.set_query("inactive", window, cx));
    });
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    probe.read_with(&cx, |probe, _| {
        assert_eq!(probe.saved, ["global", "alice", "amy", "ava"]);
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    probe.read_with(&cx, |probe, cx| {
        assert_eq!(
            probe.commits.len(),
            4,
            "dismissal must not commit a second time"
        );
        assert!(
            probe
                .governance
                .participant_picker
                .read(cx)
                .query(cx)
                .is_empty()
        );
        assert!(
            participant_picker_change(
                &ComboboxEvent::Change(vec!["global".to_owned()]),
                &probe.accounts,
                Some("other-wallet"),
                Some("wallet"),
                &probe.saved
            )
            .is_none(),
            "a stale picker must not change the next wallet"
        );
    });
}

#[gpui::test]
fn staking_table_actions_and_stake_disclosures_fit_at_compact_width(cx: &TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx
        .update(|app| {
            app.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(0.0), px(0.0)),
                        size: size(STAKING_TABLE_MIN_WIDTH, px(120.0)),
                    })),
                    ..Default::default()
                },
                |window, cx| {
                    let table = cx.new(|cx| {
                        TableState::new(TableProbeDelegate::new(), window, cx)
                            .sortable(false)
                            .row_selectable(true)
                            .col_selectable(false)
                            .col_movable(false)
                            .col_resizable(false)
                    });
                    cx.new(|_| TableRenderProbe {
                        table,
                        table_height: STAKING_OVERVIEW_ROW_HEIGHT,
                        external_scrollbar: false,
                    })
                },
            )
        })
        .expect("open staking table probe window");
    let mut cx = VisualTestContext::from_window(*window, cx);
    cx.refresh().expect("refresh staking table probe window");
    cx.run_until_parked();

    let viewport_bounds = cx
        .debug_bounds("staking-visible-probe-viewport")
        .expect("staking visible probe viewport bounds");
    assert_eq!(viewport_bounds.size.height, STAKING_OVERVIEW_ROW_HEIGHT);
    assert!(cx.debug_bounds("staking-test-external-scrollbar").is_none());

    let assert_widths = |available_width: f32, expected: [gpui::Pixels; 5]| {
        let layout = staking_table_layout(px(available_width));
        assert_eq!(layout.columns.len(), expected.len());
        assert_eq!(layout.compact, available_width < 720.0);
        for ((_, column), expected) in layout.columns.iter().zip(expected) {
            assert_eq!(column.width, expected);
        }
    };
    assert_widths(480.0, [px(76.0), px(120.0), px(88.0), px(118.0), px(224.0)]);
    assert_widths(626.0, [px(76.0), px(120.0), px(88.0), px(118.0), px(224.0)]);
    assert_widths(
        719.0,
        [px(76.0), px(120.0), px(181.0), px(118.0), px(224.0)],
    );
    assert_widths(
        720.0,
        [px(76.0), px(120.0), px(118.0), px(118.0), px(288.0)],
    );

    let actions_bounds = cx
        .debug_bounds("staking-actions-content")
        .expect("staking Actions content bounds");
    for selector in ["staking-test-undelegate", "staking-test-unlock"] {
        let button_bounds = cx
            .debug_bounds(selector)
            .expect("staking action button bounds");
        // Stock Large Table centers 24px buttons in 40px content boxes; allow
        // 1px subpixel tolerance vertically.
        assert!(
            button_bounds.origin.x >= actions_bounds.origin.x
                && button_bounds.origin.y >= actions_bounds.origin.y - px(1.0)
                && button_bounds.origin.x + button_bounds.size.width
                    <= actions_bounds.origin.x + actions_bounds.size.width
                && button_bounds.origin.y + button_bounds.size.height
                    <= actions_bounds.origin.y + actions_bounds.size.height + px(1.0),
            "{selector} bounds {button_bounds:?} should be contained by Actions content {actions_bounds:?}"
        );
    }
    let stake_bounds = cx
        .debug_bounds("staking-stake-content")
        .expect("staking Stake content bounds");
    let trigger_bounds = cx
        .debug_bounds("staking-test-stake-trigger")
        .expect("staking stake disclosure trigger bounds");
    let stake_center_y =
        f32::from(stake_bounds.origin.y) + f32::from(stake_bounds.size.height) / 2.0;
    let trigger_center_y =
        f32::from(trigger_bounds.origin.y) + f32::from(trigger_bounds.size.height) / 2.0;
    assert!(
        trigger_bounds.origin.x >= stake_bounds.origin.x
            && trigger_bounds.origin.x + trigger_bounds.size.width
                <= stake_bounds.origin.x + stake_bounds.size.width
            && (trigger_center_y - stake_center_y).abs() <= 1.0,
        "stake trigger bounds {trigger_bounds:?} should be horizontally contained and vertically centered in Stake content {stake_bounds:?}"
    );
    let state_bounds = cx
        .debug_bounds("staking-state-content")
        .expect("staking State content bounds");
    let state_label_bounds = cx
        .debug_bounds("staking-state-label")
        .expect("staking state label bounds");
    assert!(
        state_label_bounds.origin.x >= state_bounds.origin.x
            && state_label_bounds.origin.y >= state_bounds.origin.y - px(1.0)
            && state_label_bounds.origin.x + state_label_bounds.size.width
                <= state_bounds.origin.x + state_bounds.size.width
            && state_label_bounds.origin.y + state_label_bounds.size.height
                <= state_bounds.origin.y + state_bounds.size.height + px(1.0),
        "state label bounds {state_label_bounds:?} should be contained by State content {state_bounds:?}"
    );
    for (selector, bounds) in [
        ("staking-stake-content", stake_bounds),
        ("staking-actions-content", actions_bounds),
        ("staking-state-content", state_bounds),
    ] {
        assert!(
            bounds.origin.y >= viewport_bounds.origin.y - px(1.0)
                && bounds.origin.y + bounds.size.height
                    <= viewport_bounds.origin.y + viewport_bounds.size.height + px(1.0),
            "{selector} bounds {bounds:?} should be vertically contained by visible viewport {viewport_bounds:?}"
        );
    }
}

#[gpui::test]
fn staking_table_pointer_clicks_do_not_select_rows_but_keyboard_does(cx: &TestAppContext) {
    cx.update(gpui_component::init);
    let action_clicked = Arc::new(AtomicBool::new(false));
    let callback = action_clicked.clone();
    let window = cx
        .update(|app| {
            app.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(0.0), px(0.0)),
                        size: size(STAKING_TABLE_MIN_WIDTH, px(120.0)),
                    })),
                    ..Default::default()
                },
                |window, cx| {
                    let table = cx.new(|cx| {
                        TableState::new(
                            TableProbeDelegate::with_action_callback(callback),
                            window,
                            cx,
                        )
                        .sortable(false)
                        .row_selectable(true)
                        .col_selectable(false)
                        .col_movable(false)
                        .col_resizable(false)
                    });
                    cx.new(|_| TableRenderProbe {
                        table,
                        table_height: STAKING_OVERVIEW_ROW_HEIGHT,
                        external_scrollbar: false,
                    })
                },
            )
        })
        .expect("open staking table pointer probe window");
    let mut cx = VisualTestContext::from_window(*window, cx);
    cx.refresh()
        .expect("refresh staking table pointer probe window");
    cx.run_until_parked();

    let probe = window.root(&mut cx).expect("staking pointer probe root");
    let table = cx.update(|_, app| probe.read(app).table.clone());
    let row_bounds = cx
        .debug_bounds("staking-test-row")
        .expect("staking first row bounds");
    let blank_position = point(row_bounds.origin.x + px(2.0), row_bounds.center().y);
    cx.simulate_click(blank_position, gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(cx.update(|_, app| table.read(app).selected_row()), None);

    let action_bounds = cx
        .debug_bounds("staking-test-undelegate")
        .expect("staking action button bounds");
    cx.simulate_click(action_bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(action_clicked.load(Ordering::SeqCst));
    assert_eq!(cx.update(|_, app| table.read(app).selected_row()), None);

    cx.update_window(*window, |_, window, app| {
        window.focus(&table.read(app).focus_handle(app), app);
    })
    .expect("focus staking table");
    cx.simulate_keystrokes("down");
    assert_eq!(cx.update(|_, app| table.read(app).selected_row()), Some(0));
}

#[gpui::test]
fn staking_table_external_scrollbar_tracks_full_height_and_clamps_bottom(cx: &TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx
        .update(|app| {
            app.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(0.0), px(0.0)),
                        size: size(STAKING_TABLE_MIN_WIDTH, px(200.0)),
                    })),
                    ..Default::default()
                },
                |window, cx| {
                    let table = cx.new(|cx| {
                        TableState::new(TableProbeDelegate::with_rows(6), window, cx)
                            .sortable(false)
                            .row_selectable(true)
                            .col_selectable(false)
                            .col_movable(false)
                            .col_resizable(false)
                    });
                    cx.new(|_| TableRenderProbe {
                        table,
                        table_height: px(200.0),
                        external_scrollbar: true,
                    })
                },
            )
        })
        .expect("open staking external scrollbar probe window");
    let mut cx = VisualTestContext::from_window(*window, cx);
    cx.refresh()
        .expect("refresh staking external scrollbar probe window");
    cx.run_until_parked();

    let probe = window
        .root(&mut cx)
        .expect("staking external scrollbar probe root");
    let table = cx.update(|_, app| probe.read(app).table.clone());
    let vertical_scroll_handle = cx.update(|_, app| table.read(app).vertical_scroll_handle.clone());
    let scroll_handle = vertical_scroll_handle.0.borrow().base_handle.clone();
    let viewport_bounds = cx
        .debug_bounds("staking-visible-probe-viewport")
        .expect("staking visible probe viewport bounds");
    let scrollbar_bounds = cx
        .debug_bounds("staking-test-external-scrollbar")
        .expect("staking external scrollbar bounds");
    assert_eq!(scrollbar_bounds.size.width, STAKING_TABLE_SCROLLBAR_WIDTH);
    assert_eq!(scrollbar_bounds.size.height, px(200.0));
    assert_eq!(scrollbar_bounds.origin.y, viewport_bounds.origin.y);
    assert_eq!(
        scrollbar_bounds.origin.y + scrollbar_bounds.size.height,
        viewport_bounds.origin.y + viewport_bounds.size.height
    );
    assert_eq!(
        ScrollbarHandle::content_size(&vertical_scroll_handle).height,
        px(240.0)
    );
    assert_eq!(scroll_handle.max_offset().y, px(40.0));

    ScrollbarHandle::set_offset(&vertical_scroll_handle, point(px(0.0), px(-40.0)));
    cx.update(|_, app| probe.update(app, |_, cx| cx.notify()));
    cx.refresh()
        .expect("refresh staking table at external scrollbar bottom");
    cx.run_until_parked();
    assert_eq!(scroll_handle.offset().y, px(-40.0));
    let row_six_bounds = cx
        .debug_bounds("staking-test-row-six")
        .expect("staking sixth row bounds at bottom");
    assert_eq!(row_six_bounds.size.height, STAKING_OVERVIEW_ROW_HEIGHT);
    assert!(row_six_bounds.origin.y >= viewport_bounds.origin.y - px(1.0));
    assert!(
        row_six_bounds.origin.y + row_six_bounds.size.height
            <= viewport_bounds.origin.y + viewport_bounds.size.height + px(1.0)
    );
    assert!(
        (row_six_bounds.origin.y + row_six_bounds.size.height)
            >= viewport_bounds.origin.y + viewport_bounds.size.height - px(1.0)
    );

    ScrollbarHandle::set_offset(&vertical_scroll_handle, point(px(0.0), px(-40.0)));
    cx.update(|_, app| probe.update(app, |_, cx| cx.notify()));
    cx.refresh()
        .expect("refresh staking table at repeated external scrollbar bottom");
    cx.run_until_parked();
    assert_eq!(scroll_handle.offset().y, px(-40.0));
}

#[gpui::test]
fn staking_account_header_handles_compact_and_wide_metrics(cx: &TestAppContext) {
    cx.update(gpui_component::init);
    for (compact, width, height, prefix) in [
        (true, px(514.0), px(260.0), "staking-account-header"),
        (false, px(960.0), px(180.0), "staking-mid-account-header"),
    ] {
        if !compact {
            assert!(!staking_account_header_compact(width, false));
        }
        let window = cx
            .update(|app| {
                app.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: point(px(0.0), px(0.0)),
                            size: size(width, height),
                        })),
                        ..Default::default()
                    },
                    |_window, cx| {
                        cx.new(|_| AccountHeaderProbe {
                            compact,
                            width,
                            height,
                            prefix,
                        })
                    },
                )
            })
            .expect("open staking account header probe window");
        let mut cx = VisualTestContext::from_window(*window, cx);
        cx.refresh()
            .expect("refresh staking account header probe window");
        cx.run_until_parked();

        let card = cx
            .debug_bounds("staking-account-header-card")
            .expect("staking account header card bounds");
        let (identity_selector, metric_selector) = if compact {
            (
                "staking-account-header-identity",
                "staking-account-header-metric-0",
            )
        } else {
            (
                "staking-mid-account-header-identity",
                "staking-mid-account-header-metric-0",
            )
        };
        let identity = cx
            .debug_bounds(identity_selector)
            .expect("staking account identity bounds");
        let metric = cx
            .debug_bounds(metric_selector)
            .expect("voting power metric bounds");
        if compact {
            assert!(identity.origin.y + identity.size.height <= metric.origin.y + px(1.0));
            assert!(metric.origin.x >= card.origin.x);
            assert!(metric.origin.y >= card.origin.y);
            assert!(metric.origin.x + metric.size.width <= card.origin.x + card.size.width);
            assert!(metric.origin.y + metric.size.height <= card.origin.y + card.size.height);
            assert!(identity.origin.x >= card.origin.x);
            assert!(identity.origin.y >= card.origin.y);
            assert!(identity.origin.x + identity.size.width <= card.origin.x + card.size.width);
            assert!(identity.origin.y + identity.size.height <= card.origin.y + card.size.height);
        } else {
            let card_padding = px(12.0);
            assert!(metric.origin.x >= card.origin.x + card_padding);
            assert!(
                metric.origin.x + metric.size.width
                    <= card.origin.x + card.size.width - card_padding
            );
            assert!(identity.origin.x >= card.origin.x + card_padding);
            assert!(identity.origin.x + identity.size.width <= metric.origin.x);
        }
    }
}
