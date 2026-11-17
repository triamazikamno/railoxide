use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    Bounds, Render, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, point, size,
};

use super::*;
use gpui_component::scroll::ScrollbarHandle;

struct ParticipantPickerProbe {
    governance: GovernanceState,
    accounts: Vec<PublicAccountMetadata>,
    saved: Vec<String>,
    commits: Vec<Vec<String>>,
}

impl ParticipantPickerProbe {
    fn sync_picker(&mut self, window: &mut Window, cx: &mut App) {
        let selected = self.saved.iter().cloned().collect();
        let items = participant_choices(&self.accounts, &selected, Some("wallet"), "");
        self.governance.sync_participant_picker(
            Some("wallet".to_owned()),
            items,
            &self.saved,
            window,
            cx,
        );
    }
}

impl Render for ParticipantPickerProbe {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        self.sync_picker(window, cx);
        div()
            .p_3()
            .child(Combobox::new(&self.governance.participant_picker).w(px(220.0)))
    }
}

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

struct TableProbeDelegate {
    columns: Vec<(StakeTableColumnKind, Column)>,
    compact: bool,
    row_count: usize,
    action_clicked: Option<Arc<AtomicBool>>,
}

impl TableProbeDelegate {
    fn new() -> Self {
        Self::with_rows(5)
    }

    fn with_rows(row_count: usize) -> Self {
        let layout = staking_table_layout(STAKING_TABLE_MIN_WIDTH);
        Self {
            compact: layout.compact,
            columns: layout.columns,
            row_count,
            action_clicked: None,
        }
    }

    fn with_action_callback(action_clicked: Arc<AtomicBool>) -> Self {
        let mut delegate = Self::new();
        delegate.action_clicked = Some(action_clicked);
        delegate
    }
}

impl TableDelegate for TableProbeDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.row_count
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].1.clone()
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("header")
            .absolute()
            .h(px(0.0))
            .max_h(px(0.0))
            .border_0()
            .overflow_hidden()
    }

    fn render_th(
        &mut self,
        _col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        div().h(px(0.0)).max_h(px(0.0)).overflow_hidden()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        let row = render_staking_table_row(row_ix);
        if row_ix == 5 {
            row.debug_selector(|| "staking-test-row-six".to_owned())
        } else if row_ix == 0 {
            row.debug_selector(|| "staking-test-row".to_owned())
        } else {
            row
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        if self.columns[col_ix].0 == StakeTableColumnKind::Stake {
            div()
                .debug_selector(|| "staking-stake-content".to_owned())
                .h_full()
                .flex()
                .items_center()
                .child(
                    Popover::new(SharedString::from(format!(
                        "staking-test-stake-popover-{row_ix}"
                    )))
                    .trigger(
                        Button::new(SharedString::from(format!(
                            "staking-test-stake-trigger-{row_ix}"
                        )))
                        .debug_selector(|| "staking-test-stake-trigger".to_owned())
                        .text()
                        .xsmall()
                        .child("#123…90"),
                    )
                    .content(|_state, _window, _cx| div().child("stake details")),
                )
        } else if self.columns[col_ix].0 == StakeTableColumnKind::Actions {
            div()
                .debug_selector(|| "staking-actions-content".to_owned())
                .h_full()
                .flex()
                .items_center()
                .justify_end()
                .gap_1()
                .child(
                    app_button_base(SharedString::from(format!(
                        "staking-test-undelegate-{row_ix}"
                    )))
                    .debug_selector(|| "staking-test-undelegate".to_owned())
                    .small()
                    .outline()
                    .child("Undelegate")
                    .on_click({
                        let action_clicked = self.action_clicked.clone();
                        move |_event, _window, _cx| {
                            if let Some(action_clicked) = &action_clicked {
                                action_clicked.store(true, Ordering::SeqCst);
                            }
                        }
                    }),
                )
                .child(
                    app_button_base(SharedString::from(format!("staking-test-unlock-{row_ix}")))
                        .debug_selector(|| "staking-test-unlock".to_owned())
                        .small()
                        .outline()
                        .child(if self.compact {
                            "Unlock"
                        } else {
                            "Unlock · 2 steps"
                        }),
                )
        } else if self.columns[col_ix].0 == StakeTableColumnKind::State {
            div()
                .debug_selector(|| "staking-state-content".to_owned())
                .h_full()
                .flex()
                .items_center()
                .child(
                    div()
                        .debug_selector(|| "staking-state-label".to_owned())
                        .child(app_status_tag("Unlocking", theme::WARNING)),
                )
        } else {
            div().size_full()
        }
    }
}

struct TableRenderProbe {
    table: Entity<TableState<TableProbeDelegate>>,
    table_height: gpui::Pixels,
    external_scrollbar: bool,
}

impl Render for TableRenderProbe {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let vertical_scroll_handle = self.table.read(cx).vertical_scroll_handle.clone();
        let viewport = div()
            .debug_selector(|| "staking-visible-probe-viewport".to_owned())
            .w_full()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .child(
                DataTable::new(&self.table)
                    .large()
                    .bordered(false)
                    .scrollbar_visible(false, false),
            );
        div()
            .relative()
            .w_full()
            .h(self.table_height)
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(viewport)
            .when(self.external_scrollbar, |this| {
                this.child(
                    div()
                        .occlude()
                        .debug_selector(|| "staking-test-external-scrollbar".to_owned())
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .w(STAKING_TABLE_SCROLLBAR_WIDTH)
                        .child(Scrollbar::vertical(&vertical_scroll_handle)),
                )
            })
    }
}

struct AccountHeaderProbe {
    compact: bool,
    width: gpui::Pixels,
    height: gpui::Pixels,
    prefix: &'static str,
}

impl Render for AccountHeaderProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        let identity = div()
            .flex()
            .items_center()
            .child(app_strong_text("Imported account"));
        div()
            .debug_selector(|| "staking-account-header-card".to_owned())
            .w(self.width)
            .h(self.height)
            .p_3()
            .child(render_staking_account_header(
                identity,
                voting_power_metric("123456789012345678901234 RAIL", false, None),
                self.compact,
                Some(self.prefix),
            ))
    }
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

fn account(uuid: &str, status: PublicAccountStatus, global: bool) -> PublicAccountMetadata {
    PublicAccountMetadata {
        public_account_uuid: uuid.to_owned(),
        address: Address::from([uuid.as_bytes()[0]; 20]),
        label: Some(uuid.to_owned()),
        source: wallet_ops::vault::PublicAccountSource::Imported,
        scope: if global {
            wallet_ops::vault::PublicAccountScope::Global
        } else {
            wallet_ops::vault::PublicAccountScope::PrivateWallet {
                wallet_uuid: "wallet".to_owned(),
            }
        },
        derivation_index: None,
        hardware_descriptor: None,
        status,
        display_order: 0,
    }
}

fn key(wallet: &str, chain: u64, uuid: &str, address: Address) -> GovernanceContextKey {
    GovernanceContextKey {
        wallet_id: Some(wallet.to_owned()),
        chain_id: chain,
        participants: vec![GovernanceParticipantIdentity {
            uuid: uuid.to_owned(),
            address,
        }],
    }
}

fn positive_reward(token: Address, amount: U256) -> RewardView {
    RewardView::Positive {
        amount,
        evidence: Box::new(RewardEvidence {
            token,
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: Vec::new(),
            amount,
        }),
    }
}

#[test]
fn priced_positive_reward_has_formatted_usd_value() {
    let token = Address::from([1; 20]);
    let cache = TokenAnchorRateCache::new();
    cache.store_rate(1, token, U256::from(10).pow(U256::from(18)));
    cache.store_native_usd_rate(1, U256::from(2_000_000), 18);

    assert_eq!(
        reward_usd_label(
            1,
            token,
            Some(&positive_reward(
                token,
                U256::from(3) * U256::from(10).pow(U256::from(18)),
            )),
            &cache,
        ),
        Some("$6.00".to_owned())
    );
}

#[test]
fn resolved_reward_total_adds_rows_and_zero_needs_no_price() {
    let priced_token = Address::from([1; 20]);
    let zero_token = Address::from([2; 20]);
    let cache = TokenAnchorRateCache::new();
    cache.store_rate(1, priced_token, U256::from(10).pow(U256::from(18)));
    cache.store_native_usd_rate(1, U256::from(2_000_000), 18);
    let rewards = BTreeMap::from([
        (
            (String::from("account"), priced_token),
            positive_reward(
                priced_token,
                U256::from(3) * U256::from(10).pow(U256::from(18)),
            ),
        ),
        ((String::from("account"), zero_token), RewardView::Zero),
    ]);
    let resolved_reward_keys = rewards.keys().cloned().collect();

    assert_eq!(
        reward_usd_total(
            1,
            "account",
            &[priced_token, zero_token],
            &rewards,
            &resolved_reward_keys,
            &cache,
        ),
        RewardUsdState::Value(U256::from(6_000_000))
    );
    assert_eq!(
        reward_usd_label(
            1,
            zero_token,
            rewards.get(&(String::from("account"), zero_token)),
            &cache,
        ),
        Some("$0.00".to_owned())
    );
}

#[test]
fn reward_total_fails_closed_for_unresolved_or_overflowed_rows() {
    let first = Address::from([1; 20]);
    let second = Address::from([2; 20]);
    let unpriced = Address::from([3; 20]);
    let cache = TokenAnchorRateCache::new();
    cache.store_rate(1, first, U256::ONE);
    cache.store_rate(1, second, U256::ONE);
    cache.store_native_usd_rate(1, U256::ONE, 18);
    let unpriced_reward = positive_reward(unpriced, U256::ONE);
    assert_eq!(
        reward_usd_label(1, unpriced, Some(&unpriced_reward), &cache),
        Some("USD unavailable".to_owned())
    );

    let cases = [
        (BTreeMap::new(), &[first][..], RewardUsdState::Loading),
        (
            BTreeMap::from([(
                (String::from("account"), first),
                RewardView::Unavailable("failed".into()),
            )]),
            &[first][..],
            RewardUsdState::Unavailable,
        ),
        (
            BTreeMap::from([(
                (String::from("account"), unpriced),
                positive_reward(unpriced, U256::ONE),
            )]),
            &[unpriced][..],
            RewardUsdState::Unavailable,
        ),
    ];
    for (rewards, tokens, expected) in cases {
        let resolved_reward_keys = rewards.keys().cloned().collect();
        assert_eq!(
            reward_usd_total(
                1,
                "account",
                tokens,
                &rewards,
                &resolved_reward_keys,
                &cache,
            ),
            expected
        );
    }

    let rewards = BTreeMap::from([
        (
            (String::from("account"), first),
            positive_reward(first, U256::MAX),
        ),
        (
            (String::from("account"), second),
            positive_reward(second, U256::MAX),
        ),
    ]);
    assert_eq!(
        reward_usd_total(
            1,
            "account",
            &[first, second],
            &rewards,
            &rewards.keys().cloned().collect(),
            &cache,
        ),
        RewardUsdState::Unavailable
    );
}

#[test]
fn staking_formatter_uses_deployment_symbols_and_effective_override() {
    let amount = U256::from(15) * U256::from(10).pow(U256::from(17));
    let empty_registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    for (chain_id, symbol) in [(1, "RAIL"), (56, "RAILBSC"), (137, "RAILPOLY")] {
        let token = governance_contracts(chain_id)
            .expect("supported governance deployment")
            .governance_token;
        assert_eq!(
            format_staking_amount(chain_id, token, amount, &empty_registry),
            format!("1.5 {symbol}")
        );
    }

    let token = governance_contracts(1)
        .expect("supported governance deployment")
        .governance_token;
    let override_registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([(
            (1, token.to_string().to_ascii_lowercase()),
            wallet_ops::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: token.to_string(),
                symbol: "CUSTOM".to_owned(),
                decimals: 18,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]),
    };
    assert_eq!(
        format_staking_amount(1, token, amount, &override_registry),
        "1.5 CUSTOM"
    );
}

#[test]
fn staking_account_projection_tracks_received_voting_power() {
    let owner = Address::from([1; 20]);
    let external = Address::from([2; 20]);
    let stakes = vec![
        StakePosition {
            owner,
            id: U256::ZERO,
            delegate: owner,
            amount: U256::from(10),
            staketime: U256::ZERO,
            locktime: U256::ZERO,
            claimed_time: U256::ZERO,
            state: StakeState::Active,
        },
        StakePosition {
            owner,
            id: U256::from(1),
            delegate: external,
            amount: U256::from(5),
            staketime: U256::ZERO,
            locktime: U256::ZERO,
            claimed_time: U256::ZERO,
            state: StakeState::Unlocking,
        },
        StakePosition {
            owner,
            id: U256::from(2),
            delegate: external,
            amount: U256::from(7),
            staketime: U256::ZERO,
            locktime: U256::ZERO,
            claimed_time: U256::ZERO,
            state: StakeState::Claimable,
        },
        StakePosition {
            owner,
            id: U256::from(3),
            delegate: external,
            amount: U256::from(11),
            staketime: U256::ZERO,
            locktime: U256::ZERO,
            claimed_time: U256::from(1),
            state: StakeState::Claimed,
        },
    ];

    let projection = staking_account_projection(&stakes);
    assert_eq!(projection.unclaimed_principal, U256::from(22));
    assert_eq!(projection.active_self_delegated, U256::from(10));
    assert_eq!(projection.active_external_delegated, U256::ZERO);
    assert_eq!(
        projection.received_voting_power(U256::from(30)),
        U256::from(20)
    );
}

#[test]
fn stale_generation_and_key_results_are_rejected() {
    let address = Address::from([1; 20]);
    let mut state = StakingReadState::default();
    let first = key("wallet", 1, "a", address);
    let generation = state.begin(first.clone());
    let second = key("wallet", 1, "b", address);
    let next_generation = state.begin(second.clone());
    assert!(!state.apply_account(&first, generation, "a".into(), Err(Arc::from("stale"))));
    assert!(state.apply_account(
        &second,
        next_generation,
        "b".into(),
        Err(Arc::from("account"))
    ));
    assert!(!state.accounts.contains_key("a"));
}

#[test]
fn reward_evidence_cache_requires_current_identity_tokens_and_ttl() {
    let address = Address::from([1; 20]);
    let token = Address::from([2; 20]);
    let token_two = Address::from([3; 20]);
    let key = key("wallet", 1, "a", address);
    let mut state = StakingReadState::default();
    let generation = state.begin(key.clone());
    assert!(state.apply_global(
        &key,
        generation,
        Ok(StakingGlobalMetrics {
            total_staked: U256::ZERO,
            total_voting_power: U256::ZERO,
            deploy_time: U256::ZERO,
            snapshot_interval: U256::ONE,
            current_interval: U256::ZERO,
            stake_locktime: U256::ZERO,
            chain_time: U256::ZERO,
        })
    ));
    assert!(state.apply_reward(
        &key,
        generation,
        "a".into(),
        token,
        Ok(Some(RewardEvidence {
            token,
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: Vec::new(),
            amount: U256::ONE,
        }))
    ));
    assert!(state.apply_bulk_reward(
        &key,
        generation,
        "a".into(),
        vec![token, token_two],
        &Ok(Some(wallet_ops::RewardBatchEvidence {
            reward_tokens: vec![token, token_two],
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: vec![Vec::new(), Vec::new()],
            expected_amounts: vec![U256::ONE, U256::ZERO],
        })),
    ));
    let now = Instant::now();
    assert!(
        state
            .cached_reward_evidence_at(&key, "a", address, &[token], now)
            .is_some()
    );
    assert!(
        state
            .cached_reward_evidence_at(&key, "a", address, &[token, token_two], now)
            .is_some()
    );
    assert!(
        state
            .cached_reward_evidence_at(&key, "missing", address, &[token], now)
            .is_none()
    );
    assert!(
        state
            .cached_reward_evidence_at(&key, "a", address, &[token_two, token], now)
            .is_none()
    );
    assert!(
        state
            .cached_reward_evidence_at(
                &key,
                "a",
                address,
                &[token],
                now + STAKING_REWARD_EVIDENCE_TTL + Duration::from_secs(1),
            )
            .is_none()
    );
    state.begin(key.clone());
    assert!(
        state
            .cached_reward_evidence_at(&key, "a", address, &[token], Instant::now())
            .is_none()
    );
}

fn zero_staking_metrics() -> StakingGlobalMetrics {
    StakingGlobalMetrics {
        total_staked: U256::ZERO,
        total_voting_power: U256::ZERO,
        deploy_time: U256::ZERO,
        snapshot_interval: U256::ONE,
        current_interval: U256::ZERO,
        stake_locktime: U256::ZERO,
        chain_time: U256::ZERO,
    }
}

#[test]
fn cached_bulk_reward_evidence_is_projected_onto_the_selected_subset() {
    let address = Address::from([1; 20]);
    let first = Address::from([2; 20]);
    let second = Address::from([3; 20]);
    let third = Address::from([4; 20]);
    let key = key("wallet", 1, "a", address);
    let mut state = StakingReadState::default();
    let generation = state.begin(key.clone());
    assert!(state.apply_global(&key, generation, Ok(zero_staking_metrics())));
    assert!(state.apply_bulk_reward(
        &key,
        generation,
        "a".into(),
        vec![first, second, third],
        &Ok(Some(wallet_ops::RewardBatchEvidence {
            reward_tokens: vec![first, second, third],
            starting_interval: U256::ZERO,
            ending_interval: U256::ONE,
            staking_intervals: vec![U256::ZERO, U256::ONE],
            hints: vec![U256::ZERO, U256::ONE],
            claimed_intervals: vec![Vec::new(), vec![U256::ONE], Vec::new()],
            expected_amounts: vec![U256::from(5), U256::ZERO, U256::ZERO],
        })),
    ));
    let now = Instant::now();

    let projected = state
        .cached_reward_evidence_at(&key, "a", address, &[first, third], now)
        .expect("cached evidence projected onto the selection");
    assert_eq!(projected.reward_tokens, vec![first, third]);
    assert_eq!(projected.expected_amounts, vec![U256::from(5), U256::ZERO]);
    assert_eq!(
        projected.claimed_intervals,
        vec![Vec::<U256>::new(), Vec::<U256>::new()]
    );
    assert_eq!(projected.starting_interval, U256::ZERO);
    assert_eq!(projected.ending_interval, U256::ONE);
    assert_eq!(projected.hints, vec![U256::ZERO, U256::ONE]);
    assert!(
        state
            .cached_reward_evidence_at(&key, "a", address, &[second, third], now)
            .is_none()
    );
}

#[test]
fn reward_cost_presentation_prices_and_flags_below_fee_rows() {
    let chain = 1;
    let cache = TokenAnchorRateCache::new();
    let gas_cost = U256::from(10).pow(U256::from(15));
    let unpriced_native = reward_cost_presentation(
        chain,
        gas_cost,
        RewardUsdState::Value(U256::from(1_000)),
        &cache,
    );
    assert_eq!(unpriced_native.usd, None);
    assert_eq!(
        unpriced_native.label,
        crate::root::tokens::format_native_token_amount_for_display(chain, gas_cost)
    );
    assert!(!unpriced_native.below_fee);

    cache.store_native_usd_rate(
        chain,
        U256::from(2_000) * U256::from(10).pow(U256::from(6)),
        18,
    );
    let fee_usd = cache
        .cached_native_usd_micro_value(chain, gas_cost)
        .expect("native fee is priced");
    let below = reward_cost_presentation(
        chain,
        gas_cost,
        RewardUsdState::Value(fee_usd - U256::ONE),
        &cache,
    );
    assert_eq!(below.usd, Some(fee_usd));
    assert_eq!(below.label, format_usd_micro_value(fee_usd));
    assert!(below.below_fee);
    assert!(
        !reward_cost_presentation(chain, gas_cost, RewardUsdState::Value(fee_usd), &cache)
            .below_fee
    );
    assert!(
        !reward_cost_presentation(chain, gas_cost, RewardUsdState::Unavailable, &cache).below_fee
    );
}

#[test]
fn default_claim_selection_uses_marginal_fees_instead_of_standalone_costs() {
    let chain = 1;
    let address = Address::from([1; 20]);
    let covered = Address::from([2; 20]);
    let below = Address::from([3; 20]);
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(
        chain,
        U256::from(2_000) * U256::from(10).pow(U256::from(6)),
        18,
    );
    cache.store_rate(chain, covered, U256::from(10).pow(U256::from(18)));
    cache.store_rate(chain, below, U256::from(10).pow(U256::from(18)));
    let key = key("wallet", chain, "a", address);
    let mut state = StakingReadState::default();
    let generation = state.begin(key.clone());
    assert!(state.apply_global(&key, generation, Ok(zero_staking_metrics())));
    // A whole native token of reward covers the fee; a ten-thousandth of one does not.
    for (token, amount) in [
        (covered, U256::from(10).pow(U256::from(18))),
        (below, U256::from(10).pow(U256::from(14))),
    ] {
        assert!(state.apply_reward(
            &key,
            generation,
            "a".into(),
            token,
            Ok(Some(RewardEvidence {
                token,
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                staking_intervals: vec![U256::ZERO],
                hints: vec![U256::ZERO],
                claimed_intervals: Vec::new(),
                amount,
            })),
        ));
    }

    let mut estimate = RewardSelectionEstimate {
        evidence: None,
        steps: Vec::new(),
        expected_fee_per_gas: 1_000_000_000,
        added_gas: BTreeMap::new(),
        shared_gas: Some(500_000),
    };
    // The small reward cannot cover its standalone fee, but covers its added batch fee.
    for (covered_gas, below_gas, expected) in [
        (Some(50_000), Some(50_000), vec![covered, below]),
        (Some(50_000), Some(200_000), vec![covered]),
        (Some(50_000), None, vec![covered, below]),
        (Some(2_000_000_000), Some(200_000), vec![]),
        (None, None, vec![covered, below]),
    ] {
        estimate.added_gas = BTreeMap::from([(covered, covered_gas), (below, below_gas)]);
        assert_eq!(
            default_reward_claim_selection_for(
                &state,
                chain,
                "a",
                &[below, covered],
                &estimate,
                None,
                &cache
            ),
            expected,
            "added gas: covered={covered_gas:?}, below={below_gas:?}",
        );
    }
}

#[test]
fn reward_claim_costs_reprice_cached_gas_with_the_current_quote() {
    let step = |estimated_gas| wallet_ops::RewardClaimStep {
        reward_tokens: Vec::new(),
        starting_interval: U256::ZERO,
        ending_interval: U256::ZERO,
        subtotal: U256::ZERO,
        expected_amounts: Vec::new(),
        estimated_gas,
    };

    let steps = [step(120_000), step(90_000)];
    let quote = wallet_ops::SelfBroadcastGasFeeQuote {
        observed_fee_model: None,
        rpc_gas_price: 8,
        current_base_fee_per_gas: Some(8),
        suggested_max_fee_per_gas: 20,
        suggested_max_priority_fee_per_gas: 2,
    };
    let custom = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 20,
        max_priority_fee_per_gas: 2,
    };
    // Auto must use the refreshed quote, just like the displayed gas controls.
    let auto_price = reward_claim_fee_per_gas(PublicActionGasFeeSelection::Auto, Some(quote), 3);
    assert_eq!(
        reward_selection_cost(&steps, auto_price),
        Some(U256::from(2_310_000))
    );
    let custom_price = reward_claim_fee_per_gas(custom, Some(quote), 3);
    assert_eq!(
        reward_selection_cost(&steps, custom_price),
        Some(
            U256::from(210_000)
                * U256::from(wallet_ops::expected_eip1559_fee_per_gas(quote, 20, 2))
        )
    );
    assert_eq!(custom_price, auto_price);
    // Without a quote the maximum fee is the conservative estimate, using the same gas.
    assert_eq!(
        reward_selection_cost(&steps, reward_claim_fee_per_gas(custom, None, 3)),
        Some(U256::from(4_200_000))
    );
    assert_eq!(reward_selection_cost(&[step(u64::MAX), step(1)], 1), None);
}

#[test]
fn reward_claim_marginals_exclude_shared_work_and_reconcile_the_selected_batch() {
    let first = Address::from([1; 20]);
    let second = Address::from([2; 20]);
    let empty = 520_000;
    let first_alone = 1_272_000;
    let second_alone = 1_274_000;
    let combined = 1_746_000;
    let added_first = reward_claim_added_gas(true, combined, second_alone);
    let added_second = reward_claim_added_gas(false, first_alone, combined);
    assert_eq!(added_first, Some(472_000));
    assert_eq!(added_second, Some(474_000));
    let mut added = BTreeMap::from([(first, added_first), (second, added_second)]);
    let shared = reward_claim_shared_gas(&[first, second], Some(combined), &added).unwrap();
    assert_eq!(shared, 800_000);
    assert_eq!(
        shared + added_first.unwrap() + added_second.unwrap(),
        combined
    );
    assert_eq!(
        reward_claim_shared_gas(&[], Some(empty), &added),
        Some(empty)
    );
    added.insert(first, reward_claim_added_gas(true, first_alone, empty));
    assert_eq!(
        reward_claim_shared_gas(&[first], Some(first_alone), &added),
        Some(empty)
    );
    // A failed or inconsistent comparison is unavailable, never a fake zero fee.
    added.insert(first, None);
    assert_eq!(
        reward_claim_shared_gas(&[first], Some(first_alone), &added),
        None
    );
    assert_eq!(reward_claim_added_gas(true, first_alone, combined), None);
    assert_eq!(reward_claim_added_gas(false, combined, first_alone), None);
}

#[gpui::test]
fn reward_gas_editor_validates_custom_inputs_and_resets_for_next_claim(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    cx.add_empty_window().update(|window, app| {
        let editor = app.new(|cx| Eip1559GasFeeEditorState::new(window, cx));
        editor.update(app, |editor, cx| {
            editor.mode = Eip1559GasFeeMode::Custom;
            editor
                .max_fee_input
                .update(cx, |input, cx| input.set_value("2", window, cx));
            editor
                .max_priority_fee_input
                .update(cx, |input, cx| input.set_value("3", window, cx));
            assert!(editor.selection(cx).is_err());
            editor
                .max_priority_fee_input
                .update(cx, |input, cx| input.set_value("1", window, cx));
            assert_eq!(
                editor.selection(cx),
                Ok(PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas: 2_000_000_000,
                    max_priority_fee_per_gas: 1_000_000_000,
                })
            );
            editor.refreshing = true;
            let stale_refresh = editor.refresh_id;
            editor.reset_for_request(window, cx);
            assert_eq!(editor.selection(cx), Ok(PublicActionGasFeeSelection::Auto));
            assert_ne!(editor.refresh_id, stale_refresh);
            assert!(!editor.refreshing);
            // Switching back to Custom cannot silently reuse the previous claim's caps.
            editor.mode = Eip1559GasFeeMode::Custom;
            assert!(editor.selection(cx).is_err());
        });
    });
}

fn reward_claim_flow(kind: StakingActionKind) -> GovernanceActionFlowState {
    let mut flow = GovernanceActionFlowState::default();
    flow.set_staking_selection(StakingActionSelection {
        actor_uuid: String::from("a"),
        actor: Address::from([1; 20]),
        kind,
    });
    flow
}

#[test]
fn reward_claim_selection_toggles_stay_unique_and_ascending() {
    let first = Address::from([2; 20]);
    let second = Address::from([3; 20]);
    let third = Address::from([4; 20]);
    let mut flow = reward_claim_flow(StakingActionKind::RewardClaimSelected {
        tokens: vec![second],
    });

    assert!(flow.set_reward_claim_selection_token(third, true));
    assert!(flow.set_reward_claim_selection_token(first, true));
    assert!(flow.set_reward_claim_selection_token(first, true));
    assert_eq!(
        flow.reward_claim_selection_tokens(),
        Some(&[first, second, third][..])
    );
    assert!(flow.set_reward_claim_selection_token(second, false));
    assert!(flow.set_reward_claim_selection_token(second, false));
    assert_eq!(
        flow.reward_claim_selection_tokens(),
        Some(&[first, third][..])
    );
    assert!(flow.set_reward_claim_selection_all(vec![third, first, third]));
    assert_eq!(
        flow.reward_claim_selection_tokens(),
        Some(&[first, third][..])
    );

    let mut stake_flow = reward_claim_flow(StakingActionKind::Stake);
    assert!(!stake_flow.set_reward_claim_selection_token(second, true));
    assert_eq!(stake_flow.reward_claim_selection_tokens(), None);
}

#[test]
fn stale_reward_selection_estimates_are_discarded() {
    let token = Address::from([2; 20]);
    let mut flow = reward_claim_flow(StakingActionKind::RewardClaimSelected {
        tokens: vec![token],
    });

    let generation = flow.begin_reward_selection_estimate();
    assert_eq!(
        flow.reward_selection_estimate,
        Some(RewardSelectionEstimateState::Pending)
    );
    assert!(
        !flow.apply_reward_selection_estimate(generation.wrapping_sub(1), Err(Arc::from("stale")),)
    );
    assert_eq!(
        flow.reward_selection_estimate,
        Some(RewardSelectionEstimateState::Pending)
    );
    assert!(flow.apply_reward_selection_estimate(generation, Err(Arc::from("unavailable"))));
    assert_eq!(
        flow.reward_selection_estimate,
        Some(RewardSelectionEstimateState::Unavailable(Arc::from(
            "unavailable"
        )))
    );

    assert!(flow.set_reward_claim_selection_token(token, false));
    let emptied = flow.begin_reward_selection_estimate();
    assert_ne!(emptied, generation);
    assert_eq!(
        flow.reward_selection_estimate,
        Some(RewardSelectionEstimateState::Pending)
    );
    assert!(!flow.apply_reward_selection_estimate(generation, Err(Arc::from("old selection"))));
}

#[test]
fn reward_evidence_mode_prefers_the_reviewed_selection_estimate() {
    let first = Address::from([2; 20]);
    let second = Address::from([3; 20]);
    let evidence = |tokens: Vec<Address>| wallet_ops::RewardBatchEvidence {
        expected_amounts: vec![U256::ONE; tokens.len()],
        claimed_intervals: vec![Vec::new(); tokens.len()],
        reward_tokens: tokens,
        starting_interval: U256::ZERO,
        ending_interval: U256::ZERO,
        staking_intervals: vec![U256::ZERO],
        hints: vec![U256::ZERO],
    };
    let estimate = RewardSelectionEstimateState::Estimated(Box::new(RewardSelectionEstimate {
        evidence: Some(evidence(vec![first])),
        steps: vec![wallet_ops::RewardClaimStep {
            reward_tokens: vec![first],
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            subtotal: U256::ZERO,
            expected_amounts: vec![U256::ONE],
            estimated_gas: 120_000,
        }],
        expected_fee_per_gas: 1,
        added_gas: BTreeMap::new(),
        shared_gas: None,
    }));

    assert!(matches!(
        reward_evidence_mode_for_selection(Some(&estimate), &[first], None),
        RewardEvidenceMode::CachedInitial {
            steps: Some(steps),
            ..
        } if steps.len() == 1
    ));
    assert!(matches!(
        reward_evidence_mode_for_selection(
            Some(&estimate),
            &[first, second],
            Some(evidence(vec![first, second])),
        ),
        RewardEvidenceMode::CachedInitial { steps: None, .. }
    ));
    assert!(matches!(
        reward_evidence_mode_for_selection(
            Some(&RewardSelectionEstimateState::Pending),
            &[first],
            None,
        ),
        RewardEvidenceMode::Fresh
    ));
}

#[test]
fn scoped_successes_survive_account_and_asset_failures() {
    let address = Address::from([1; 20]);
    let token = Address::from([2; 20]);
    let token_two = Address::from([3; 20]);
    let mut state = StakingReadState::default();
    let key = GovernanceContextKey {
        wallet_id: Some(String::from("wallet")),
        chain_id: 1,
        participants: vec![
            GovernanceParticipantIdentity {
                uuid: String::from("a"),
                address,
            },
            GovernanceParticipantIdentity {
                uuid: String::from("good"),
                address,
            },
            GovernanceParticipantIdentity {
                uuid: String::from("pending"),
                address: Address::from([8; 20]),
            },
        ],
    };
    let generation = state.begin(key.clone());
    let metrics = StakingGlobalMetrics {
        total_staked: U256::from(3),
        total_voting_power: U256::from(2),
        deploy_time: U256::ZERO,
        snapshot_interval: U256::from(1),
        current_interval: U256::ZERO,
        stake_locktime: U256::ZERO,
        chain_time: U256::ZERO,
    };
    assert!(state.apply_global(&key, generation, Ok(metrics.clone())));
    assert!(state.apply_account(
        &key,
        generation,
        "a".into(),
        Err(Arc::from("account failed"))
    ));
    assert!(state.apply_reward(
        &key,
        generation,
        "a".into(),
        token,
        Ok(Some(RewardEvidence {
            token,
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: Vec::new(),
            amount: U256::from(1),
        }))
    ));
    assert!(state.apply_account(
        &key,
        generation,
        "good".into(),
        Ok(AccountStakeResult {
            account: address,
            voting_power: Ok(U256::from(4)),
            balance: Ok(U256::from(5)),
            stakes: Ok(Vec::new()),
        })
    ));
    assert!(state.apply_reward(
        &key,
        generation,
        "good".into(),
        token,
        Ok(Some(RewardEvidence {
            token,
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: Vec::new(),
            amount: U256::from(1),
        }))
    ));
    assert!(state.apply_reward(&key, generation, "good".into(), token_two, Ok(None),));
    assert!(state.apply_bulk_reward(
        &key,
        generation,
        "a".into(),
        vec![token, token_two],
        &Err(Arc::from("bulk asset failed")),
    ));
    assert!(state.apply_bulk_reward(
        &key,
        generation,
        "good".into(),
        vec![token, token_two],
        &Ok(Some(wallet_ops::RewardBatchEvidence {
            reward_tokens: vec![token, token_two],
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: vec![Vec::new(), Vec::new()],
            expected_amounts: vec![U256::from(1), U256::ZERO],
        })),
    ));
    assert_eq!(state.metrics.as_ref(), Some(&metrics));
    assert!(state.accounts["a"].stakes.is_err());
    assert!(matches!(
        state.rewards[&(String::from("a"), token)],
        RewardView::Positive { .. }
    ));
    assert!(matches!(
        state.rewards[&(String::from("good"), token)],
        RewardView::Positive { .. }
    ));
    assert_eq!(state.status, StakingRefreshStatus::Loading);
    assert!(state.global_action_ready());
    assert!(state.account_action_ready("good"));
    assert!(!state.account_action_ready("a"));
    assert!(!state.account_action_ready("pending"));
    assert!(state.reward_action_ready("good", token));
    assert!(state.reward_action_ready("a", token));
    assert!(!state.reward_action_ready("pending", token));
    let claim_selection =
        |uuid: &str, actor: Address, tokens: Vec<Address>| StakingActionSelection {
            actor_uuid: uuid.to_owned(),
            actor,
            kind: StakingActionKind::RewardClaimSelected { tokens },
        };
    assert!(state.action_selection_ready(&claim_selection("good", address, vec![token])));
    assert!(state.action_selection_ready(&claim_selection("good", address, Vec::new())));
    assert!(!state.action_selection_ready(&claim_selection(
        "good",
        address,
        vec![token, token_two]
    )));
    assert!(!state.action_selection_ready(&claim_selection(
        "pending",
        Address::from([8; 20]),
        Vec::new()
    )));
    assert!(!state.action_selection_ready(&StakingActionSelection {
        actor_uuid: String::from("a"),
        actor: address,
        kind: StakingActionKind::Delegate {
            stake_id: U256::ZERO,
        },
    }));
    assert!(state.apply_reward(
        &key,
        generation,
        "good".into(),
        token_two,
        Err(Arc::from("second asset failed")),
    ));
    assert!(state.action_selection_ready(&claim_selection("good", address, vec![token])));
    assert!(!state.action_selection_ready(&claim_selection("good", address, vec![token_two])));
    assert!(!state.action_selection_ready(&StakingActionSelection {
        actor_uuid: String::from("good"),
        actor: Address::from([9; 20]),
        kind: StakingActionKind::RewardClaimSelected {
            tokens: vec![token]
        },
    }));

    let next_generation = state.begin(key.clone());
    assert_ne!(generation, next_generation);
    assert_eq!(state.metrics.as_ref(), Some(&metrics));
    assert!(state.accounts.contains_key("good"));
    assert!(state.rewards.contains_key(&(String::from("good"), token)));
    assert!(!state.global_action_ready());
    assert!(!state.account_action_ready("good"));
    assert!(!state.reward_action_ready("good", token));
    assert!(!state.apply_bulk_reward(
        &key,
        generation,
        "good".into(),
        vec![token, token_two],
        &Ok(Some(wallet_ops::RewardBatchEvidence {
            reward_tokens: vec![token, token_two],
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: vec![Vec::new(), Vec::new()],
            expected_amounts: vec![U256::from(1), U256::ZERO],
        })),
    ));
    assert!(!state.action_selection_ready(&claim_selection("good", address, vec![token])));

    assert!(state.apply_global(
        &key,
        next_generation,
        Err(Arc::from("global refresh failed"))
    ));
    assert_eq!(state.status, StakingRefreshStatus::Stale);
    assert!(!state.global_action_ready());
}

#[test]
fn empty_participants_finish_fresh_after_global_metrics() {
    let mut state = StakingReadState::default();
    let key = GovernanceContextKey {
        wallet_id: Some(String::from("wallet")),
        chain_id: 1,
        participants: Vec::new(),
    };
    let generation = state.begin(key.clone());
    let metrics = StakingGlobalMetrics {
        total_staked: U256::from(3),
        total_voting_power: U256::from(2),
        deploy_time: U256::ZERO,
        snapshot_interval: U256::from(1),
        current_interval: U256::ZERO,
        stake_locktime: U256::ZERO,
        chain_time: U256::from(10),
    };
    assert!(state.apply_global(&key, generation, Ok(metrics.clone())));
    assert!(state.finish(&key, generation));
    assert_eq!(state.status, StakingRefreshStatus::Fresh);
    assert_eq!(state.metrics, Some(metrics));
    assert!(state.accounts.is_empty());
    assert!(state.rewards.is_empty());
}

#[test]
fn participant_states_cover_scopes_inactive_search_and_empty_results() {
    let accounts = vec![
        account("a", PublicAccountStatus::Active, false),
        account("b", PublicAccountStatus::Inactive, false),
        account("g", PublicAccountStatus::Active, true),
        account("i", PublicAccountStatus::Inactive, true),
    ];
    let selected = BTreeSet::from([String::from("b"), String::from("g")]);
    let summary = participant_summary(&accounts, &selected, Some("wallet"));
    assert_eq!(
        summary,
        ParticipantSummary {
            selected: 2,
            inactive: 1
        }
    );
    let choices = participant_choices(&accounts, &selected, Some("wallet"), "");
    assert_eq!(
        choices
            .iter()
            .map(|choice| choice.uuid.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "g"]
    );
    assert!(!choices.iter().any(|choice| choice.uuid == "i"));
    assert_eq!(
        participant_choices(&accounts, &selected, Some("wallet"), "missing"),
        [] as [ParticipantChoice; 0]
    );
    assert_eq!(
        participant_choices(&accounts, &BTreeSet::new(), Some("wallet"), "alice").len(),
        0
    );
    assert_eq!(
        participant_choices(&accounts, &selected, Some("other"), "")
            .iter()
            .map(|choice| choice.uuid.as_str())
            .collect::<Vec<_>>(),
        vec!["g"]
    );
}

#[test]
fn default_tab_and_context_invalidation_are_explicit() {
    assert_eq!(GovernanceTab::default(), GovernanceTab::Proposals);
    let mut state = StakingReadState::default();
    let key = key("wallet", 1, "a", Address::from([1; 20]));
    let generation = state.begin(key.clone());
    state.invalidate();
    assert!(!state.finish(&key, generation));
    assert_eq!(state.status, StakingRefreshStatus::Idle);
}

#[test]
fn staking_control_ids_bind_actor_subject_and_action() {
    let first = staking_control_id("actor-a", "reward-claim", "token");
    let second = staking_control_id("actor-b", "reward-claim", "token");
    let different_action = staking_control_id("actor-a", "delegate", "token");

    assert_ne!(first, second);
    assert_ne!(first, different_action);
}

fn reward_metadata(
    staking_deploy_time: U256,
    distribution_interval: U256,
) -> GovernorRewardsIntervalMetadata {
    GovernorRewardsIntervalMetadata {
        multiplier: U256::ONE,
        staking_deploy_time,
        distribution_interval,
        current_interval: U256::ZERO,
        next_earmark_intervals: BTreeMap::new(),
    }
}

#[test]
fn reward_interval_countdown_uses_fourteen_day_boundaries() {
    let fourteen_days = U256::from(14 * 24 * 60 * 60);
    let metadata = reward_metadata(U256::from(100), fourteen_days);
    let countdown = reward_interval_countdown(
        &metadata,
        U256::from(100) + fourteen_days * U256::from(3) + U256::from(42),
    )
    .expect("deployed reward interval");

    assert_eq!(countdown.interval, U256::from(3));
    assert_eq!(
        countdown.boundary,
        U256::from(100) + fourteen_days * U256::from(4)
    );
    assert_eq!(
        remaining_reward_interval_seconds(
            countdown,
            U256::from(100) + fourteen_days * U256::from(3) + U256::from(42),
        ),
        Some(fourteen_days.to::<u64>() - 42)
    );
}

#[test]
fn reward_interval_countdown_rejects_zero_interval_and_predeployment_time() {
    let zero_interval = reward_metadata(U256::from(100), U256::ZERO);
    assert!(reward_interval_countdown(&zero_interval, U256::from(100)).is_none());

    let metadata = reward_metadata(U256::from(100), U256::from(14));
    assert!(reward_interval_countdown(&metadata, U256::from(99)).is_none());
}

#[test]
fn reward_interval_countdown_rejects_checked_arithmetic_overflow() {
    let increment_overflow = reward_metadata(U256::ZERO, U256::ONE);
    assert!(reward_interval_countdown(&increment_overflow, U256::MAX).is_none());

    let multiplication_overflow = reward_metadata(U256::ZERO, U256::MAX);
    assert!(reward_interval_countdown(&multiplication_overflow, U256::MAX).is_none());

    let addition_overflow = reward_metadata(U256::MAX, U256::ONE);
    assert!(reward_interval_countdown(&addition_overflow, U256::MAX).is_none());
}

#[test]
fn governance_boundary_crossing_uses_the_stored_reward_boundary() {
    let metadata = reward_metadata(U256::from(10), U256::from(5));
    let countdown =
        reward_interval_countdown(&metadata, U256::from(20)).expect("deployed reward interval");
    let state = StakingReadState {
        reward_interval_countdown: Some(countdown),
        ..StakingReadState::default()
    };

    assert!(!governance_boundary_crossed(&state, U256::from(24)));
    assert!(governance_boundary_crossed(&state, U256::from(25)));
}

#[test]
fn reward_interval_state_is_generation_scoped_and_cleared_on_refresh() {
    let address = Address::from([1; 20]);
    let key = key("wallet", 1, "a", address);
    let mut state = StakingReadState::default();
    let generation = state.begin(key.clone());
    let countdown = RewardIntervalCountdown {
        interval: U256::ZERO,
        boundary: U256::ONE,
    };
    assert!(state.apply_reward_interval(&key, generation, Some(countdown)));

    let next_generation = state.begin(key.clone());
    assert_ne!(generation, next_generation);
    assert_eq!(state.reward_interval_countdown, None);
    assert!(!state.apply_reward_interval(&key, generation, Some(countdown)));
    assert!(state.apply_reward_interval(&key, next_generation, Some(countdown)));
    state.invalidate();
    assert_eq!(state.reward_interval_countdown, None);
}

#[test]
fn governance_time_tick_new_generation_supersedes_previous_owner() {
    let (owner, armed) = arm_governance_time_tick(None, 10);
    assert_eq!(owner, Some(10));
    assert!(armed);

    let (owner, armed) = arm_governance_time_tick(owner, 10);
    assert_eq!(owner, Some(10));
    assert!(!armed);

    let (owner, armed) = arm_governance_time_tick(owner, 11);
    assert_eq!(owner, Some(11));
    assert!(armed);
}

#[test]
fn stale_governance_time_tick_claim_leaves_new_owner_intact() {
    let (owner, claimed) = claim_governance_time_tick(Some(11), 10);
    assert_eq!(owner, Some(11));
    assert!(!claimed);
}

#[test]
fn governance_time_tick_owner_claims_only_once() {
    let (owner, claimed) = claim_governance_time_tick(Some(11), 11);
    assert_eq!(owner, None);
    assert!(claimed);

    let (owner, claimed) = claim_governance_time_tick(owner, 11);
    assert_eq!(owner, None);
    assert!(!claimed);
}

#[test]
fn non_crossed_governance_time_tick_claim_can_rearm_generation() {
    let (owner, claimed) = claim_governance_time_tick(Some(11), 11);
    assert!(claimed);
    assert_eq!(owner, None);

    let (owner, armed) = arm_governance_time_tick(owner, 11);
    assert_eq!(owner, Some(11));
    assert!(armed);
}

#[test]
fn unlock_boundary_is_checked_at_equality_and_ignores_terminal_history() {
    let owner = Address::from([4; 20]);
    let mut state = StakingReadState::default();
    state.accounts.insert(
        "a".into(),
        StakingAccountView {
            voting_power: Ok(U256::ZERO),
            balance: Ok(U256::ZERO),
            stakes: Ok(vec![
                StakePosition {
                    owner,
                    id: U256::ZERO,
                    delegate: owner,
                    amount: U256::from(1),
                    staketime: U256::ZERO,
                    locktime: U256::from(20),
                    claimed_time: U256::ZERO,
                    state: StakeState::Unlocking,
                },
                StakePosition {
                    owner,
                    id: U256::from(1),
                    delegate: owner,
                    amount: U256::from(1),
                    staketime: U256::ZERO,
                    locktime: U256::from(10),
                    claimed_time: U256::from(11),
                    state: StakeState::Claimed,
                },
            ]),
        },
    );
    assert_eq!(nearest_unlock_boundary(&state), Some(U256::from(20)));
    assert!(!governance_boundary_crossed(&state, U256::from(19)));
    assert!(governance_boundary_crossed(&state, U256::from(20)));
    state.invalidate();
    assert!(!governance_boundary_crossed(&state, U256::from(20)));
}

fn isolated_test_route() -> wallet_ops::ClassRoute {
    wallet_ops::RpcChainRoute::new(1, vec![reqwest::Url::parse("http://127.0.0.1:1").unwrap()])
        .for_class(wallet_ops::RpcTrafficClass::Sensitive)
        .unwrap()
        .isolated(wallet_ops::OperationHttpClient::for_tests(
            reqwest::Client::new(),
            wallet_ops::OperationNetworkIsolation::Unavailable(
                wallet_ops::WalletNetworkMode::Direct,
            ),
        ))
}

#[tokio::test]
async fn participant_reads_each_get_one_participant_and_their_own_route() {
    let jitters = std::cell::Cell::new(0);
    let mut reads = Vec::new();
    read_participants_isolated(
        vec!["a", "b"],
        async || jitters.set(jitters.get() + 1),
        || true,
        async || Ok(isolated_test_route()),
        async |participant, route: Result<wallet_ops::ClassRoute, String>| {
            reads.push((participant, route.unwrap()));
        },
    )
    .await;
    assert_eq!(jitters.get(), 1);
    let mut participants = reads
        .iter()
        .map(|(participant, _)| *participant)
        .collect::<Vec<_>>();
    participants.sort_unstable();
    assert_eq!(participants, ["a", "b"]);
    assert_ne!(reads[0].1, reads[1].1);
}

#[tokio::test]
async fn failed_route_open_fails_only_that_participant() {
    let opens = std::cell::Cell::new(0);
    let mut reads = Vec::new();
    read_participants_isolated(
        vec!["a", "b"],
        async || {},
        || true,
        async || {
            opens.set(opens.get() + 1);
            if opens.get() == 1 {
                Err("isolated route unavailable".to_owned())
            } else {
                Ok(isolated_test_route())
            }
        },
        async |participant, route: Result<wallet_ops::ClassRoute, String>| {
            reads.push((participant, route.is_ok()));
        },
    )
    .await;
    assert_eq!(reads.len(), 2);
    assert_ne!(reads[0].0, reads[1].0);
    assert_eq!([reads[0].1, reads[1].1], [false, true]);
}

#[tokio::test]
async fn superseded_participant_refresh_stops_before_the_next_participant() {
    let a = GovernanceParticipantIdentity {
        uuid: "a".to_owned(),
        address: Address::from([1; 20]),
    };
    let b = GovernanceParticipantIdentity {
        uuid: "b".to_owned(),
        address: Address::from([2; 20]),
    };
    let proposal_key =
        |participants: Vec<GovernanceParticipantIdentity>| ProposalParticipationKey {
            version: wallet_ops::GovernanceContractVersion::V2,
            contract: Address::from([9; 20]),
            index: U256::ZERO,
            context: GovernanceContextKey {
                wallet_id: Some("wallet".to_owned()),
                chain_id: 1,
                participants,
            },
        };
    let key_ab = proposal_key(vec![a.clone(), b.clone()]);
    let key_a = proposal_key(vec![a.clone()]);
    let state = std::cell::RefCell::new(ProposalParticipationState::default());
    let g1 = state.borrow_mut().begin(&key_ab, &[a.address, b.address]);
    let g2 = std::cell::Cell::new(None);
    let opens = std::cell::Cell::new(0);
    let mut reads = Vec::new();
    read_participants_isolated(
        vec![a.address, b.address],
        async || {},
        || state.borrow().current(&key_ab, g1),
        async || {
            opens.set(opens.get() + 1);
            Ok(isolated_test_route())
        },
        async |account, _route: Result<wallet_ops::ClassRoute, String>| {
            reads.push(account);
            // The participant set changes while the first participant is being read.
            g2.set(Some(state.borrow_mut().begin(&key_a, &[a.address])));
        },
    )
    .await;
    assert_eq!(opens.get(), 1);
    assert_eq!(reads.len(), 1);

    let mut state = state.into_inner();
    let g2 = g2.get().unwrap();
    assert!(!state.apply(&key_ab, g1, b.address, Err(Arc::from("late"))));
    assert!(state.apply(&key_a, g2, a.address, Err(Arc::from("route unavailable"))));
    let proposal = wallet_ops::GovernanceProposal {
        contract_version: key_a.version,
        index: key_a.index,
        contract_address: key_a.contract,
        proposer: Address::ZERO,
        proposal_document: String::new(),
        publish_time: U256::ONE,
        vote_call_time: U256::ZERO,
        sponsorship: U256::ZERO,
        executed: false,
        yay_votes: U256::ZERO,
        nay_votes: U256::ZERO,
        sponsor_snapshot_interval: U256::ONE,
        voting_snapshot_interval: U256::ZERO,
        actions: Vec::new(),
    };
    assert!(matches!(
        state.row(&proposal, &a.address),
        Some(super::super::governance_action::ProposalParticipationRow::Unavailable(_))
    ));
    // The superseded request left no stuck `Loading` row for `b`.
    assert!(state.row(&proposal, &b.address).is_none());
    assert!(!state.loading());
}

#[test]
fn an_action_drops_the_actors_participation_on_every_proposal_of_its_contract() {
    use super::super::governance_action::ProposalParticipationRow;
    use wallet_ops::GovernanceContractVersion::{V1, V2};

    let actor = Address::from([1; 20]);
    let other = Address::from([2; 20]);
    let key = |chain_id: u64, version, contract: u8, index: u64| ProposalParticipationKey {
        version,
        contract: Address::from([contract; 20]),
        index: U256::from(index),
        context: GovernanceContextKey {
            wallet_id: Some("wallet".to_owned()),
            chain_id,
            participants: [("actor", actor), ("other", other)]
                .map(|(uuid, address)| GovernanceParticipantIdentity {
                    uuid: uuid.to_owned(),
                    address,
                })
                .into(),
        },
    };
    let proposal = |key: &ProposalParticipationKey| wallet_ops::GovernanceProposal {
        contract_version: key.version,
        index: key.index,
        contract_address: key.contract,
        proposer: Address::ZERO,
        proposal_document: String::new(),
        publish_time: U256::ONE,
        vote_call_time: U256::ZERO,
        sponsorship: U256::ZERO,
        executed: false,
        yay_votes: U256::ZERO,
        nay_votes: U256::ZERO,
        sponsor_snapshot_interval: U256::ONE,
        voting_snapshot_interval: U256::ZERO,
        actions: Vec::new(),
    };
    let snapshot = |interval| wallet_ops::GovernanceAccountSnapshot {
        interval,
        voting_power: U256::ZERO,
        hint: U256::ZERO,
    };
    let participation =
        |key: &ProposalParticipationKey, account| wallet_ops::GovernanceParticipation {
            proposal_version: key.version,
            proposal_id: key.index,
            voting_contract: key.contract,
            account,
            current_voting_power: U256::ZERO,
            sponsorship_snapshot: snapshot(U256::ONE),
            voting_snapshot: snapshot(U256::ZERO),
            sponsored: U256::ZERO,
            voted: U256::ZERO,
            last_sponsored: wallet_ops::GovernanceLastSponsored {
                last_sponsor_time: U256::ZERO,
                proposal_id: U256::ZERO,
            },
        };
    let ready = |state: &mut ProposalParticipationState,
                 key: &ProposalParticipationKey,
                 account: Address| {
        state.show(key);
        matches!(
            state.row(&proposal(key), &account),
            Some(ProposalParticipationRow::Ready(_))
        )
    };

    let acted = key(1, V2, 9, 0);
    let sibling = key(1, V2, 9, 1);
    let other_version = key(1, V1, 9, 0);
    let other_contract = key(1, V2, 8, 0);
    let other_chain = key(5, V2, 9, 0);
    let mut state = ProposalParticipationState::default();
    for loaded in [
        &acted,
        &sibling,
        &other_version,
        &other_contract,
        &other_chain,
    ] {
        let generation = state.begin(loaded, &[actor, other]);
        for account in [actor, other] {
            assert!(state.apply(
                loaded,
                generation,
                account,
                Ok(participation(loaded, account))
            ));
        }
    }
    // `lastSponsored` is per account and voting contract, so the actor's row on another
    // proposal of the same contract is as stale as on the acted one.
    state.invalidate_actor_on_contract(&acted, actor);
    assert!(!ready(&mut state, &acted, actor));
    assert!(!ready(&mut state, &sibling, actor));
    assert!(ready(&mut state, &sibling, other));
    assert!(ready(&mut state, &other_version, actor));
    assert!(ready(&mut state, &other_contract, actor));
    assert!(ready(&mut state, &other_chain, actor));

    // The actor's read in flight on another proposal of the contract cannot restore a
    // pre-action row, while the other participant's read still completes.
    let mut state = ProposalParticipationState::default();
    let generation = state.begin(&sibling, &[actor, other]);
    state.invalidate_actor_on_contract(&acted, actor);
    assert!(!state.apply(
        &sibling,
        generation,
        actor,
        Ok(participation(&sibling, actor))
    ));
    assert!(state.row(&proposal(&sibling), &actor).is_none());
    assert!(state.apply(
        &sibling,
        generation,
        other,
        Ok(participation(&sibling, other))
    ));
    assert!(ready(&mut state, &sibling, other));

    // After a called vote, the action's read and a refresh started later both carry the new
    // voting interval. The later read wins, even when it completes first.
    let called = wallet_ops::GovernanceProposal {
        voting_snapshot_interval: U256::from(7),
        ..proposal(&acted)
    };
    let fresh = |account, power: u64| wallet_ops::GovernanceParticipation {
        current_voting_power: U256::from(power),
        voting_snapshot: snapshot(U256::from(7)),
        ..participation(&acted, account)
    };
    let power = |state: &ProposalParticipationState,
                 proposal: &wallet_ops::GovernanceProposal,
                 account: Address| match state.row(proposal, &account) {
        Some(ProposalParticipationRow::Ready(participation)) => {
            Some(participation.current_voting_power)
        }
        _ => None,
    };
    let mut state = ProposalParticipationState::default();
    let action = state.invalidate_actor_on_contract(&acted, actor);
    let generation = state.begin(&acted, &[actor, other]);
    assert!(state.apply(&acted, generation, actor, Ok(fresh(actor, 2))));
    assert!(!state.apply_action_read(1, &called, actor, action, fresh(actor, 1)));
    assert_eq!(power(&state, &called, actor), Some(U256::from(2)));

    // An action's read completing first replaces the actor's row of a request still running.
    let mut state = ProposalParticipationState::default();
    let action = state.invalidate_actor_on_contract(&acted, actor);
    state.begin(&acted, &[actor, other]);
    assert!(state.apply_action_read(1, &called, actor, action, fresh(actor, 1)));
    assert_eq!(power(&state, &called, actor), Some(U256::from(1)));

    // A request from a page cached before the vote reads the old interval. It does not
    // replace the fresh row, which that page does not request again.
    let generation = state.begin(&acted, &[actor, other]);
    for account in [actor, other] {
        assert!(state.apply(
            &acted,
            generation,
            account,
            Ok(participation(&acted, account))
        ));
    }
    assert_eq!(power(&state, &called, actor), Some(U256::from(1)));
    assert!(state.row(&proposal(&acted), &actor).is_none());
    assert_eq!(state.unloaded(&proposal(&acted)), [] as [Address; 0]);

    // A later action on the contract retires an earlier action's read still outstanding, and
    // so does the end of the wallet session.
    let mut state = ProposalParticipationState::default();
    let earlier = state.invalidate_actor_on_contract(&acted, actor);
    state.invalidate_actor_on_contract(&sibling, actor);
    assert!(!state.apply_action_read(1, &called, actor, earlier, fresh(actor, 1)));
    let outstanding = state.invalidate_actor_on_contract(&acted, actor);
    state.clear_session();
    assert!(!state.apply_action_read(1, &called, actor, outstanding, fresh(actor, 1)));
    state.show(&acted);
    assert!(state.row(&called, &actor).is_none());
}

#[test]
fn failed_participant_fails_only_its_own_cells() {
    let tokens = [Address::from([2; 20]), Address::from([3; 20])];
    let key = GovernanceContextKey {
        wallet_id: Some("wallet".to_owned()),
        chain_id: 1,
        participants: vec![
            GovernanceParticipantIdentity {
                uuid: "a".to_owned(),
                address: Address::from([1; 20]),
            },
            GovernanceParticipantIdentity {
                uuid: "b".to_owned(),
                address: Address::from([4; 20]),
            },
        ],
    };
    let mut state = StakingReadState::default();
    let generation = state.begin(key.clone());
    let error: Arc<str> = Arc::from("route unavailable");

    state.fail_participant(
        &key,
        generation.wrapping_sub(1),
        "a".into(),
        &tokens,
        Arc::clone(&error),
    );
    assert!(state.accounts.is_empty() && state.current_account_ids.is_empty());
    assert!(state.rewards.is_empty() && state.current_reward_keys.is_empty());
    assert!(state.bulk_reward_readiness.is_empty());

    state.fail_participant(&key, generation, "a".into(), &tokens, error);
    let account = &state.accounts["a"];
    assert!(account.voting_power.is_err() && account.balance.is_err() && account.stakes.is_err());
    for token in tokens {
        assert!(matches!(
            state.resolved_reward("a", token),
            Some(RewardView::Unavailable(_))
        ));
        assert!(state.current_reward_keys.contains(&("a".to_owned(), token)));
    }
    assert_eq!(
        state.bulk_reward_readiness.get("a"),
        Some(&BulkRewardReadiness {
            tokens: tokens.to_vec(),
            ready: false,
        })
    );
    assert!(!state.accounts.contains_key("b"));
    assert!(state.rewards.keys().all(|(uuid, _)| uuid == "a"));
    assert!(!state.bulk_reward_readiness.contains_key("b"));
}

alloy::sol! {
    interface GovernanceRpcFixture {
        function stakingToken() external view returns (address);
        function staking() external view returns (address);
        function proposalsLength() external view returns (uint256);
        function SPONSOR_WINDOW() external view returns (uint256);
        function VOTING_START_OFFSET() external view returns (uint256);
        function VOTING_YAY_END_OFFSET() external view returns (uint256);
        function VOTING_NAY_END_OFFSET() external view returns (uint256);
        function EXECUTION_START_OFFSET() external view returns (uint256);
        function EXECUTION_END_OFFSET() external view returns (uint256);
        function proposals(uint256 index) external view returns (
            bool executed,
            address proposer,
            string proposalDocument,
            uint256 publishTime,
            uint256 voteCallTime,
            uint256 sponsorship,
            uint256 yayVotes,
            uint256 nayVotes,
            uint256 sponsorInterval,
            uint256 votingInterval
        );
        function getActions(uint256 id) external view returns (tuple(address, bytes, uint256)[]);
        function accountSnapshotLength(address owner) external view returns (uint256);
        function accountSnapshotAt(address owner, uint256 index, uint256 hint)
            external view returns (uint256 interval, uint256 votingPower);
        function lastSponsored(address account)
            external view returns (uint256 lastSponsorTime, uint256 proposalID);
        function getSponsored(uint256 proposalID, address account)
            external view returns (uint256);
        function getClaimed(address account, address token, uint256 interval)
            external view returns (bool);
        function calculateRewards(
            address[] tokens,
            address account,
            uint256 startingInterval,
            uint256 endingInterval,
            uint256[] hints,
            bool ignoreClaimed
        ) external view returns (uint256[]);
    }
}

/// One JSON-RPC call seen by the governance RPC mock.
#[derive(Debug)]
struct SeenRpcCall {
    endpoint: &'static str,
    client: String,
    method: String,
}

/// Chain state the governance RPC mock serves, which a test may change while it runs.
struct GovernanceMockChain {
    /// Proposals on the voting contract.
    proposals: std::sync::atomic::AtomicU64,
    voting_interval: std::sync::atomic::AtomicU64,
    /// Requests tagged with this client wait until it is cleared.
    held_client: std::sync::Mutex<Option<String>>,
    released: std::sync::Condvar,
}

/// A chain whose only General endpoint and only Sensitive endpoint are separate mocks, with a
/// shared wallet client and an action's client that tag their requests.
struct GovernanceRpcMock {
    seen: Arc<std::sync::Mutex<Vec<SeenRpcCall>>>,
    chain: Arc<GovernanceMockChain>,
    effective_chain: wallet_ops::settings::EffectiveChainConfig,
    http: wallet_ops::HttpContext,
    client: wallet_ops::OperationHttpClient,
    isolated: wallet_ops::ClassRoute,
}

impl GovernanceRpcMock {
    fn take(&self) -> Vec<SeenRpcCall> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }

    fn hold(&self, client: Option<String>) {
        *self.chain.held_client.lock().unwrap() = client;
        self.chain.released.notify_all();
    }
}

fn tagged_client(tag: &str) -> reqwest::Client {
    let headers = reqwest::header::HeaderMap::from_iter([(
        reqwest::header::HeaderName::from_static("x-test-client"),
        reqwest::header::HeaderValue::from_str(tag).unwrap(),
    )]);
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

fn governance_call_output(to: Address, input: &[u8], chain: &GovernanceMockChain) -> Vec<u8> {
    use GovernanceRpcFixture as F;
    use alloy::sol_types::{SolCall, SolValue};

    let contracts = governance_contracts(1).unwrap();
    let selector: [u8; 4] = input[..4].try_into().unwrap();
    let uint = |value: u64| U256::from(value).abi_encode();
    match selector {
        F::stakingTokenCall::SELECTOR => contracts.governance_token.abi_encode(),
        F::stakingCall::SELECTOR => contracts.staking.abi_encode(),
        F::proposalsLengthCall::SELECTOR if to == contracts.voting => {
            uint(chain.proposals.load(Ordering::SeqCst))
        }
        F::proposalsLengthCall::SELECTOR => uint(0),
        // Proposal 0 is open for sponsorship, and the actor has sponsored nothing yet.
        F::SPONSOR_WINDOWCall::SELECTOR => uint(100),
        F::getSponsoredCall::SELECTOR => uint(0),
        F::VOTING_START_OFFSETCall::SELECTOR => uint(1),
        F::VOTING_YAY_END_OFFSETCall::SELECTOR => uint(2),
        F::VOTING_NAY_END_OFFSETCall::SELECTOR => uint(3),
        F::EXECUTION_START_OFFSETCall::SELECTOR => uint(4),
        F::EXECUTION_END_OFFSETCall::SELECTOR => uint(5),
        F::proposalsCall::SELECTOR => F::proposalsCall::abi_encode_returns(&F::proposalsReturn {
            executed: false,
            proposer: Address::ZERO,
            proposalDocument: String::new(),
            publishTime: U256::ONE,
            voteCallTime: U256::ZERO,
            sponsorship: U256::ZERO,
            yayVotes: U256::ZERO,
            nayVotes: U256::ZERO,
            sponsorInterval: U256::ONE,
            votingInterval: U256::from(chain.voting_interval.load(Ordering::SeqCst)),
        }),
        F::getActionsCall::SELECTOR => F::getActionsCall::abi_encode_returns(&Vec::new()),
        F::accountSnapshotLengthCall::SELECTOR => uint(0),
        F::accountSnapshotAtCall::SELECTOR => (U256::from(1_000), U256::ONE).abi_encode(),
        F::lastSponsoredCall::SELECTOR => (U256::ZERO, U256::ZERO).abi_encode(),
        F::getClaimedCall::SELECTOR => false.abi_encode(),
        F::calculateRewardsCall::SELECTOR => {
            let call = F::calculateRewardsCall::abi_decode(input).unwrap();
            F::calculateRewardsCall::abi_encode_returns(&vec![U256::ONE; call.tokens.len()])
        }
        // Balances, voting power, reward metadata and the other governance values.
        _ => uint(1),
    }
}

fn governance_rpc_answer(
    request: &serde_json::Value,
    chain: &GovernanceMockChain,
) -> serde_json::Value {
    let result = match request["method"].as_str().unwrap_or_default() {
        "eth_call" => {
            let call = &request["params"][0];
            let to = call["to"].as_str().unwrap().parse::<Address>().unwrap();
            let input = call
                .get("input")
                .or_else(|| call.get("data"))
                .and_then(serde_json::Value::as_str)
                .unwrap()
                .parse::<alloy::primitives::Bytes>()
                .unwrap();
            Some(serde_json::json!(alloy::primitives::Bytes::from(
                governance_call_output(to, &input, chain)
            )))
        }
        // Fees are served so that an estimate reaches `eth_estimateGas`, which is refused.
        "eth_gasPrice" => Some(serde_json::json!("0x2")),
        "eth_maxPriorityFeePerGas" => Some(serde_json::json!("0x1")),
        "eth_getBlockByNumber" => Some(serde_json::json!({
            "number": "0x1",
            "hash": B256::repeat_byte(1),
            "parentHash": B256::ZERO,
            "sha3Uncles": B256::ZERO,
            "miner": Address::ZERO,
            "stateRoot": B256::ZERO,
            "transactionsRoot": B256::ZERO,
            "receiptsRoot": B256::ZERO,
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x0",
            "timestamp": "0x2",
            "extraData": "0x",
            "mixHash": B256::ZERO,
            "nonce": "0x0000000000000000",
            "baseFeePerGas": "0x1",
            "uncles": [],
            "transactions": [],
        })),
        _ => None,
    };
    match result {
        Some(result) => {
            serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        }
        None => serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {"code": -32601, "message": "not served by the governance mock"},
        }),
    }
}

/// Serves governance reads on a local port, each connection on its own thread so a held client
/// does not block the others, and records each call with the client's tag.
fn spawn_governance_rpc_endpoint(
    endpoint: &'static str,
    seen: Arc<std::sync::Mutex<Vec<SeenRpcCall>>>,
    chain: Arc<GovernanceMockChain>,
) -> reqwest::Url {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                return;
            };
            let seen = Arc::clone(&seen);
            let chain = Arc::clone(&chain);
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                let (header_end, length) = loop {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break (0, 0);
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                    if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..index]).to_ascii_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .map_or(0, |value| value.trim().parse().unwrap());
                        break (index + 4, length);
                    }
                };
                if header_end == 0 {
                    return;
                }
                while bytes.len() < header_end + length {
                    let read = stream.read(&mut buffer).unwrap();
                    bytes.extend_from_slice(&buffer[..read]);
                }
                let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
                let client = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("x-test-client:"))
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let mut held = chain.held_client.lock().unwrap();
                while held.as_deref() == Some(client.as_str()) {
                    held = chain.released.wait(held).unwrap();
                }
                drop(held);
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                let requests = body
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![body.clone()]);
                let answers = requests
                    .iter()
                    .map(|request| {
                        seen.lock().unwrap().push(SeenRpcCall {
                            endpoint,
                            client: client.clone(),
                            method: request["method"].as_str().unwrap_or_default().to_owned(),
                        });
                        governance_rpc_answer(request, &chain)
                    })
                    .collect::<Vec<_>>();
                let response = if body.is_array() {
                    serde_json::Value::Array(answers)
                } else {
                    answers.into_iter().next().unwrap()
                };
                let response = response.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
            });
        }
    });
    url
}

fn governance_rpc_mock() -> GovernanceRpcMock {
    let seen = Arc::default();
    let chain = Arc::new(GovernanceMockChain {
        proposals: 1.into(),
        voting_interval: 0.into(),
        held_client: std::sync::Mutex::default(),
        released: std::sync::Condvar::new(),
    });
    let general = spawn_governance_rpc_endpoint("general", Arc::clone(&seen), Arc::clone(&chain));
    let sensitive =
        spawn_governance_rpc_endpoint("sensitive", Arc::clone(&seen), Arc::clone(&chain));
    let only = |general, sensitive| wallet_ops::RpcEndpointRoles {
        general,
        sensitive,
        submit: sensitive,
    };
    let route = wallet_ops::RpcChainRoute::new_with_transports(
        1,
        vec![
            (
                general.into(),
                wallet_ops::RpcTransport::Wallet,
                only(true, false),
            ),
            (
                sensitive.into(),
                wallet_ops::RpcTransport::Wallet,
                only(false, true),
            ),
        ],
    );
    let mut effective_chain = wallet_ops::settings::build_effective_chain_configs(
        &wallet_ops::settings::WalletSettings::default(),
    )
    .unwrap()
    .get(1)
    .cloned()
    .unwrap();
    effective_chain.rpc_route = route.clone();
    let http = wallet_ops::HttpContext::with_rpc_client_for_tests(
        tagged_client("shared"),
        wallet_ops::WalletNetworkMode::Direct,
    );
    let client = wallet_ops::OperationHttpClient::for_tests(
        tagged_client("isolated"),
        wallet_ops::OperationNetworkIsolation::Unavailable(wallet_ops::WalletNetworkMode::Direct),
    );
    let isolated = route
        .for_class(wallet_ops::RpcTrafficClass::Sensitive)
        .unwrap()
        .isolated(client.clone());
    GovernanceRpcMock {
        seen,
        chain,
        effective_chain,
        http,
        client,
        isolated,
    }
}

#[tokio::test]
async fn action_sends_governance_reads_and_estimates_only_over_the_action_client() {
    use wallet_ops::vault::{DesktopVaultStore, KdfParams, WalletSource};
    const PASSWORD: &str = "governance action test password";

    let mock = governance_rpc_mock();
    let contracts = governance_contracts(1).unwrap();
    let actor = Address::from([7; 20]);
    let tokens = [contracts.reward_tokens[0].token];

    // Both reward runs read everything, then split the claim on each refused gas estimate
    // until no range is left to try.
    let _ = plan_reward_claim_draft(
        1,
        actor,
        &tokens,
        &mock.isolated,
        &mock.effective_chain,
        &mock.http,
        &mock.client,
        RewardEvidenceMode::Fresh,
    )
    .await;
    let fresh_reward = mock.take();
    let _ = read_reward_continuation(
        1,
        actor,
        &tokens,
        &mock.isolated,
        &mock.effective_chain,
        &mock.http,
        &mock.client,
    )
    .await;
    let continued_reward = mock.take();
    let proposal = wallet_ops::GovernanceProposal {
        contract_version: wallet_ops::GovernanceContractVersion::V2,
        index: U256::ZERO,
        contract_address: contracts.voting,
        proposer: Address::ZERO,
        proposal_document: String::new(),
        publish_time: U256::ONE,
        vote_call_time: U256::ZERO,
        sponsorship: U256::ZERO,
        executed: false,
        yay_votes: U256::ZERO,
        nay_votes: U256::ZERO,
        sponsor_snapshot_interval: U256::ONE,
        voting_snapshot_interval: U256::ZERO,
        actions: Vec::new(),
    };
    // The sponsor draft holds the action's slot, which already holds its client.
    let vault_dir = std::env::temp_dir().join(format!(
        "railoxide-governance-action-{:032x}",
        rand::random::<u128>()
    ));
    let store = Arc::new(DesktopVaultStore::open(vault_dir.clone()).unwrap());
    store
        .create_vault_with_params(PASSWORD, KdfParams::new(1024, 1, 1))
        .unwrap();
    let metadata = store
        .new_wallet_metadata(PASSWORD, "wallet", 0, WalletSource::Imported, "Wallet")
        .unwrap();
    store
        .import_wallet_mnemonic_with_metadata(
            PASSWORD,
            "wallet",
            0,
            "english",
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
            &metadata,
        )
        .unwrap();
    let view = Arc::new(store.load_view_session(PASSWORD, "wallet").unwrap());
    let sponsor = super::super::governance_action::build_governance_spend_draft(
        proposal.clone(),
        super::super::governance_action::proposal_participation_key(
            &proposal,
            GovernanceContextKey {
                wallet_id: Some("wallet".to_owned()),
                chain_id: 1,
                participants: Vec::new(),
            },
        ),
        ProposalActionSelection {
            actor,
            kind: ProposalActionKind::Sponsor,
        },
        Some(U256::ONE),
        Arc::from("wallet"),
        Arc::from("actor"),
        wallet_ops::vault::PublicAccountSource::Derived,
        view,
        Arc::clone(&store),
        mock.effective_chain.clone(),
        mock.http.clone(),
        wallet_ops::OperationHttpClientSlot::for_tests(mock.http.clone(), mock.client.clone()),
        PublicActionGasFeeSelection::Auto,
    )
    .await;
    assert!(sponsor.is_err(), "the mock refuses the gas estimate");
    let sponsor_requests = mock.take();
    drop(store);
    std::fs::remove_dir_all(vault_dir).unwrap();

    for requests in [fresh_reward, continued_reward, sponsor_requests] {
        // Chain time or the block gas limit was read, and the gas was estimated.
        for method in ["eth_getBlockByNumber", "eth_estimateGas"] {
            assert!(
                requests.iter().any(|request| request.method == method),
                "{method}: {requests:?}"
            );
        }
        for request in &requests {
            assert_eq!(
                (request.endpoint, request.client.as_str()),
                ("sensitive", "isolated"),
                "{request:?}"
            );
        }
    }
}

#[tokio::test]
async fn dashboard_general_reads_use_the_shared_general_route() {
    let mock = governance_rpc_mock();
    let general = governance_route(
        1,
        &mock.effective_chain,
        wallet_ops::RpcTrafficClass::General,
    )
    .unwrap();
    let loaded = super::super::proposals::load_initial(1, 0, &general, &mock.http).await;
    assert!(loaded.is_ok(), "{:?}", loaded.err());
    let reads = mock.take();
    assert!(
        reads
            .iter()
            .any(|read| read.method == "eth_getBlockByNumber")
    );
    assert!(reads.iter().any(|read| read.method == "eth_call"));
    for read in &reads {
        assert_eq!(
            (read.endpoint, read.client.as_str()),
            ("general", "shared"),
            "{read:?}"
        );
    }
}

/// Drives the root's tasks, letting jitter timers fire, until `done` holds.
fn settle_governance(
    root: &gpui::Entity<WalletRoot>,
    cx: &gpui::VisualTestContext,
    runtime: &tokio::runtime::Runtime,
    done: impl Fn(&WalletRoot) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        runtime.block_on(tokio::time::sleep(std::time::Duration::from_millis(10)));
        cx.run_until_parked();
        if root.read_with(cx, |root, _| done(root)) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "governance reads did not settle"
        );
        // Participation reads wait up to three seconds of jitter between accounts.
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(100));
    }
}

/// Drives the root's tasks for a while, for checks that nothing more happens.
fn drive_governance(cx: &gpui::VisualTestContext, runtime: &tokio::runtime::Runtime) {
    for _ in 0..40 {
        runtime.block_on(tokio::time::sleep(std::time::Duration::from_millis(10)));
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(100));
    }
}

/// Whether `account` has a loaded row matching `proposal` under `key`, whatever is shown.
fn has_loaded_row(
    root: &WalletRoot,
    key: &ProposalParticipationKey,
    proposal: &wallet_ops::GovernanceProposal,
    account: Address,
) -> bool {
    let mut state = root.governance.proposal_participation.clone();
    state.invalidate();
    state.show(key);
    matches!(
        state.row(proposal, &account),
        Some(super::super::governance_action::ProposalParticipationRow::Ready(_))
    )
}

/// The number of isolated clients that read from the Sensitive endpoint since the last take.
fn isolated_participation_reads(mock: &GovernanceRpcMock) -> usize {
    let calls = mock.take();
    calls
        .iter()
        .filter(|call| call.endpoint == "sensitive")
        .map(|call| {
            assert!(call.client.starts_with("isolated-"), "{call:?}");
            call.client.as_str()
        })
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

#[gpui::test]
fn proposal_participation_is_read_only_on_request(cx: &mut gpui::TestAppContext) {
    use super::super::governance_action::ProposalParticipationRow;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let path = std::env::temp_dir().join(format!(
        "railoxide-proposal-participation-{:032x}",
        rand::random::<u128>()
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let mock = governance_rpc_mock();
    // Each operation client is tagged by its open, so each isolated route is told apart.
    let opens = Arc::new(AtomicUsize::new(0));
    let fail_next_open = Arc::new(AtomicBool::new(false));
    let http = {
        let opens = Arc::clone(&opens);
        let fail_next_open = Arc::clone(&fail_next_open);
        wallet_ops::HttpContext::with_rpc_client_for_tests(
            tagged_client("shared"),
            wallet_ops::WalletNetworkMode::Direct,
        )
        .with_operation_clients_for_tests(move || {
            let open = opens.fetch_add(1, Ordering::SeqCst);
            if fail_next_open.swap(false, Ordering::SeqCst) {
                return Err(eyre::eyre!("synthetic open failure"));
            }
            Ok(wallet_ops::OperationHttpClient::for_tests(
                tagged_client(&format!("isolated-{open}")),
                wallet_ops::OperationNetworkIsolation::Unavailable(
                    wallet_ops::WalletNetworkMode::Direct,
                ),
            ))
        })
    };
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let accounts = [1_u8, 2, 3].map(Address::repeat_byte);
    let set_participants = |root: &mut WalletRoot, count: usize| {
        root.ui_state.governance_participants.insert(
            "preview".to_owned(),
            (1..=count)
                .map(|index| format!("account-{index}"))
                .collect(),
        );
    };
    root.update(cx, |root, _| {
        root.http = http;
        root.effective_chain_configs = std::iter::once(mock.effective_chain.clone()).collect();
        set_participants(root, 2);
        root.governance.tab = GovernanceTab::Staking;
    });

    // Switching to Proposals loads the list, then opening a proposal, switching back to it
    // and refreshing its page or the list read no account.
    root.update(cx, |root, cx| {
        root.select_governance_tab(GovernanceTab::Proposals, cx);
    });
    settle_governance(&root, cx, &runtime, |root| {
        root.proposals.rows(0).is_some_and(|rows| !rows.is_empty())
    });
    let identity = root.read_with(cx, |root, _| root.proposals.rows(0).unwrap()[0].identity());
    root.update_in(cx, |root, window, cx| {
        root.select_proposal(0, &identity, window, cx);
        root.governance.tab = GovernanceTab::Staking;
        root.select_governance_tab(GovernanceTab::Proposals, cx);
        root.refresh_selected_proposal_page(cx);
    });
    settle_governance(&root, cx, &runtime, |root| {
        root.proposals.selected_proposal().is_some()
    });
    root.update(cx, |root, cx| root.start_proposals_refresh(true, cx));
    settle_governance(&root, cx, &runtime, |root| !root.proposals.loading);
    let proposal = root.read_with(cx, |root, _| {
        root.proposals.selected_proposal().cloned().unwrap()
    });
    let row_states = |root: &WalletRoot| {
        accounts
            .iter()
            .map(|account| {
                match root
                    .governance
                    .proposal_participation
                    .row(&proposal, account)
                {
                    None => "not loaded",
                    Some(ProposalParticipationRow::Loading) => "loading",
                    Some(ProposalParticipationRow::Ready(_)) => "ready",
                    Some(ProposalParticipationRow::Unavailable(_)) => "unavailable",
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(isolated_participation_reads(&mock), 0);
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root)),
        ["not loaded"; 3]
    );

    let request = |refresh: bool, cx: &mut gpui::VisualTestContext| {
        root.update(cx, |root, cx| {
            root.request_proposal_participation(refresh, cx);
        });
        settle_governance(&root, cx, &runtime, |root| {
            !root.governance.proposal_participation.loading()
        });
        isolated_participation_reads(&mock)
    };
    // Showing participation reads each participant once, each over its own client.
    assert_eq!(request(false, cx), 2);
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root)),
        ["ready", "ready", "not loaded"]
    );

    // Reopening the proposal shows the loaded rows without reading.
    root.update_in(cx, |root, window, cx| {
        root.clear_selected_proposal(cx);
        root.select_proposal(0, &identity, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(isolated_participation_reads(&mock), 0);
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root)),
        ["ready", "ready", "not loaded"]
    );

    // An added participant is the only account the next request reads.
    root.update(cx, |root, _| {
        set_participants(root, 3);
        root.invalidate_governance_context();
        root.show_proposal_participation(&proposal);
    });
    cx.run_until_parked();
    assert_eq!(isolated_participation_reads(&mock), 0);
    assert_eq!(request(false, cx), 1);
    assert_eq!(root.read_with(cx, |root, _| row_states(root)), ["ready"; 3]);

    // Refreshing reads every participant again.
    assert_eq!(request(true, cx), 3);

    // A failed account is read again by the next request, without the loaded ones.
    fail_next_open.store(true, Ordering::SeqCst);
    assert_eq!(request(true, cx), 2);
    let states = root.read_with(cx, |root, _| row_states(root));
    assert_eq!(
        states
            .iter()
            .filter(|state| **state == "unavailable")
            .count(),
        1,
        "{states:?}"
    );
    assert_eq!(request(false, cx), 1);
    assert_eq!(root.read_with(cx, |root, _| row_states(root)), ["ready"; 3]);

    // A successful action reads its actor once over the action's own client, even when it
    // completes while another chain is selected. No participant route opens.
    let (wallet_id, wallet_generation, chain_id, key) = root.read_with(cx, |root, _| {
        (
            root.selected_wallet_id.clone().unwrap(),
            root.active_wallet_generation,
            root.last_evm_chain,
            root.governance.proposal_participation.key.clone().unwrap(),
        )
    });
    let target = GovernanceRefreshTarget::Proposal(key.clone());
    let action =
        wallet_ops::OperationHttpClientSlot::for_tests(mock.http.clone(), mock.client.clone());
    let opened = opens.load(Ordering::SeqCst);
    root.update(cx, |root, cx| {
        root.selected_network = crate::root::SelectedNetwork::Evm(chain_id + 1);
        root.last_evm_chain = chain_id + 1;
        root.invalidate_governance_context();
        root.record_governance_action_success(
            &wallet_id,
            wallet_generation,
            &target,
            &proposal,
            accounts[0],
            &mock.effective_chain,
            &action,
            cx,
        );
        assert!(!has_loaded_row(root, &key, &proposal, accounts[0]));
    });
    settle_governance(&root, cx, &runtime, |root| {
        has_loaded_row(root, &key, &proposal, accounts[0])
    });
    assert_eq!(opens.load(Ordering::SeqCst), opened);
    let sensitive = mock
        .take()
        .into_iter()
        .filter(|call| call.endpoint == "sensitive")
        .collect::<Vec<_>>();
    assert!(!sensitive.is_empty());
    assert!(
        sensitive.iter().all(|call| call.client == "isolated"),
        "{sensitive:?}"
    );
    root.update(cx, |root, _| {
        root.selected_network = crate::root::SelectedNetwork::Evm(chain_id);
        root.last_evm_chain = chain_id;
        root.invalidate_governance_context();
        root.show_proposal_participation(&proposal);
        assert_eq!(row_states(root), ["ready"; 3]);
    });

    // A wallet change discards the loaded rows, and an action from the previous wallet
    // session reads nothing.
    root.update(cx, |root, cx| {
        root.advance_active_wallet_generation();
        root.reset_wallet_scoped_state(cx);
        root.record_governance_action_success(
            &wallet_id,
            wallet_generation,
            &target,
            &proposal,
            accounts[0],
            &mock.effective_chain,
            &action,
            cx,
        );
        root.show_proposal_participation(&proposal);
    });
    drive_governance(cx, &runtime);
    assert_eq!(opens.load(Ordering::SeqCst), opened);
    assert!(mock.take().iter().all(|call| call.endpoint != "sensitive"));
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root)),
        ["not loaded"; 3]
    );

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn an_action_reads_its_actor_once_whatever_pages_land_meanwhile(cx: &mut gpui::TestAppContext) {
    use super::super::governance_action::{ProposalParticipationRow, proposal_participation_key};
    use std::sync::atomic::AtomicUsize;

    let path = std::env::temp_dir().join(format!(
        "railoxide-proposal-participation-action-{:032x}",
        rand::random::<u128>()
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let mock = governance_rpc_mock();
    let opens = Arc::new(AtomicUsize::new(0));
    let http = {
        let opens = Arc::clone(&opens);
        wallet_ops::HttpContext::with_rpc_client_for_tests(
            tagged_client("shared"),
            wallet_ops::WalletNetworkMode::Direct,
        )
        .with_operation_clients_for_tests(move || {
            let open = opens.fetch_add(1, Ordering::SeqCst);
            Ok(wallet_ops::OperationHttpClient::for_tests(
                tagged_client(&format!("isolated-{open}")),
                wallet_ops::OperationNetworkIsolation::Unavailable(
                    wallet_ops::WalletNetworkMode::Direct,
                ),
            ))
        })
    };
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let accounts = [1_u8, 2, 3].map(Address::repeat_byte);
    root.update(cx, |root, cx| {
        root.http = http;
        root.effective_chain_configs = std::iter::once(mock.effective_chain.clone()).collect();
        root.ui_state.governance_participants.insert(
            "preview".to_owned(),
            (1..=3).map(|index| format!("account-{index}")).collect(),
        );
        root.governance.tab = GovernanceTab::Staking;
        root.select_governance_tab(GovernanceTab::Proposals, cx);
    });
    settle_governance(&root, cx, &runtime, |root| {
        root.proposals.rows(0).is_some_and(|rows| !rows.is_empty())
    });
    let identity = root.read_with(cx, |root, _| root.proposals.rows(0).unwrap()[0].identity());
    root.update_in(cx, |root, window, cx| {
        root.select_proposal(0, &identity, window, cx);
    });
    let (proposal, key, wallet_id, wallet_generation) = root.read_with(cx, |root, _| {
        let proposal = root.proposals.selected_proposal().cloned().unwrap();
        let key = proposal_participation_key(&proposal, root.governance_context_key());
        (
            proposal,
            key,
            root.selected_wallet_id.clone().unwrap(),
            root.active_wallet_generation,
        )
    });
    // The proposal once its vote is called.
    let called = wallet_ops::GovernanceProposal {
        voting_snapshot_interval: U256::from(7),
        ..proposal.clone()
    };
    let row_states = |root: &WalletRoot, proposal: &wallet_ops::GovernanceProposal| {
        accounts
            .iter()
            .map(|account| {
                match root
                    .governance
                    .proposal_participation
                    .row(proposal, account)
                {
                    None => "not loaded",
                    Some(ProposalParticipationRow::Loading) => "loading",
                    Some(ProposalParticipationRow::Ready(_)) => "ready",
                    Some(ProposalParticipationRow::Unavailable(_)) => "unavailable",
                }
            })
            .collect::<Vec<_>>()
    };
    let record_success = |actor: Address, client: &str, cx: &mut gpui::VisualTestContext| {
        let operation = wallet_ops::OperationHttpClientSlot::for_tests(
            mock.http.clone(),
            wallet_ops::OperationHttpClient::for_tests(
                tagged_client(client),
                wallet_ops::OperationNetworkIsolation::Unavailable(
                    wallet_ops::WalletNetworkMode::Direct,
                ),
            ),
        );
        root.update(cx, |root, cx| {
            root.record_governance_action_success(
                &wallet_id,
                wallet_generation,
                &GovernanceRefreshTarget::Proposal(key.clone()),
                &proposal,
                actor,
                &mock.effective_chain,
                &operation,
                cx,
            );
        });
    };

    // One participant is read while the next participant's read is held at the endpoint. A
    // refresh then waits for the held read, opening no route until it is released.
    let first = opens.load(Ordering::SeqCst);
    mock.hold(Some(format!("isolated-{}", first + 1)));
    root.update(cx, |root, cx| {
        root.request_proposal_participation(false, cx);
    });
    settle_governance(&root, cx, &runtime, |_| {
        opens.load(Ordering::SeqCst) == first + 2
    });
    root.update(cx, |root, cx| {
        root.request_proposal_participation(true, cx);
    });
    drive_governance(cx, &runtime);
    assert_eq!(opens.load(Ordering::SeqCst), first + 2);
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root, &proposal)),
        ["loading"; 3]
    );
    mock.hold(None);
    settle_governance(&root, cx, &runtime, |root| {
        !root.governance.proposal_participation.loading()
    });
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root, &proposal)),
        ["ready"; 3]
    );
    let actor = accounts[0];
    let opened = opens.load(Ordering::SeqCst);
    mock.take();

    // A call-vote completes. The action reads the actor once, over its own client, and sees
    // the called vote.
    mock.chain.voting_interval.store(7, Ordering::SeqCst);
    record_success(actor, "action", cx);
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root, &proposal)),
        ["not loaded", "ready", "ready"]
    );
    settle_governance(&root, cx, &runtime, |root| {
        has_loaded_row(root, &key, &called, actor)
    });
    let calls = mock
        .take()
        .into_iter()
        .filter(|call| call.endpoint == "sensitive")
        .collect::<Vec<_>>();
    assert!(!calls.is_empty());
    for call in &calls {
        assert_eq!(call.client, "action", "{call:?}");
    }

    // A full list refresh that read the proposal before the vote was called lands afterwards.
    // Pages start no participation read, so the stale page shows the actor as not loaded
    // without asking to read it, and loses nothing.
    mock.chain.voting_interval.store(0, Ordering::SeqCst);
    root.update(cx, |root, cx| root.start_proposals_refresh(true, cx));
    settle_governance(&root, cx, &runtime, |root| {
        !root.proposals.loading && root.proposals.selected_proposal().is_some()
    });
    assert!(mock.take().iter().all(|call| call.endpoint != "sensitive"));
    root.read_with(cx, |root, _| {
        let shown = root.proposals.selected_proposal().unwrap();
        assert_eq!(shown.voting_snapshot_interval, U256::ZERO);
        assert_eq!(row_states(root, shown), ["not loaded", "ready", "ready"]);
        assert_eq!(
            root.governance.proposal_participation.unloaded(shown),
            [] as [Address; 0]
        );
    });

    // The page with the called vote shows the actor's row without another read.
    mock.chain.voting_interval.store(7, Ordering::SeqCst);
    root.update(cx, WalletRoot::refresh_selected_proposal_page);
    settle_governance(&root, cx, &runtime, |root| {
        root.proposals
            .selected_proposal()
            .is_some_and(|shown| shown.voting_snapshot_interval == U256::from(7))
    });
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root, &called)[0]),
        "ready"
    );
    assert!(mock.take().iter().all(|call| call.endpoint != "sensitive"));
    assert_eq!(opens.load(Ordering::SeqCst), opened);

    // A wallet change while an action's read is outstanding discards its result.
    let reads = calls.len();
    record_success(actor, "late", cx);
    root.update(cx, |root, cx| {
        root.advance_active_wallet_generation();
        root.reset_wallet_scoped_state(cx);
    });
    settle_governance(&root, cx, &runtime, |_| {
        mock.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.client == "late")
            .count()
            == reads
    });
    drive_governance(cx, &runtime);
    assert!(!root.read_with(cx, |root, _| has_loaded_row(root, &key, &called, actor)));

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}
