use super::*;

impl WalletSettingsEditor {
    pub(in crate::root) fn shared_string_field(
        field_id: impl Into<String>,
        editor: Entity<Self>,
        get: impl Fn(&WalletSettings) -> String + 'static,
        set: impl Fn(&mut WalletSettings, String) + 'static,
    ) -> SettingField<SharedString> {
        let field_id = field_id.into();
        let get = Rc::new(get);
        let set = Rc::new(set);
        SettingField::render(move |options, window, cx| {
            let (value, revision) = {
                let editor = editor.read(cx);
                (
                    SharedString::from(get(&editor.draft)),
                    editor.field_sync_revision,
                )
            };
            let state = window.use_keyed_state(
                SharedString::from(format!("wallet-settings-string-{field_id}")),
                cx,
                {
                    let value = value.clone();
                    let set = set.clone();
                    let editor = editor.clone();
                    move |window, cx| {
                        let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
                        let subscription = cx.subscribe_in(&input, window, {
                            let set = set.clone();
                            let editor = editor.clone();
                            move |_state: &mut SyncedStringFieldState,
                                  input,
                                  event: &InputEvent,
                                  _window,
                                  cx| {
                                if !matches!(event, InputEvent::Change) {
                                    return;
                                }
                                let value = input.read(cx).value().to_string();
                                editor.update(cx, |editor, cx| {
                                    set(&mut editor.draft, value);
                                    editor.draft_changed(cx);
                                });
                            }
                        });
                        SyncedStringFieldState {
                            input,
                            synced_revision: revision,
                            _subscription: subscription,
                        }
                    }
                },
            );

            state.update(cx, |state, cx| {
                if state.synced_revision == revision {
                    return;
                }
                state.synced_revision = revision;
                if state.input.read(cx).value().as_ref() == value.as_ref() {
                    return;
                }
                state
                    .input
                    .update(cx, |input, cx| input.set_value(value.clone(), window, cx));
            });

            let input = state.read(cx).input.clone();
            settings_text_input(&input)
                .with_size(options.size())
                .map(|this| {
                    if matches!(options.layout(), Axis::Horizontal) {
                        this.w_64()
                    } else {
                        this.w_full()
                    }
                })
        })
    }

    pub(in crate::root) fn settings_switch_item(
        row_id: impl Into<String>,
        label: impl Into<String>,
        editor: Entity<Self>,
        _icon_chain_id: Option<u64>,
        get: impl Fn(&WalletSettings) -> bool + 'static,
        set: impl Fn(&mut WalletSettings, bool) + 'static,
    ) -> SettingItem {
        let row_id = row_id.into();
        let label = label.into();
        let get = Rc::new(get);
        let set = Rc::new(set);
        SettingItem::new(
            label,
            SettingField::<SharedString>::render(move |options, _window, cx| {
                let checked = get(&editor.read(cx).draft);
                let set_editor = editor.clone();
                let set_from_switch = set.clone();
                div()
                    .id(SharedString::from(row_id.clone()))
                    .flex()
                    .items_center()
                    .child(
                        Switch::new(SharedString::from(format!("{row_id}-switch")))
                            .checked(checked)
                            .with_size(options.size())
                            .on_click(move |enabled, _window, cx| {
                                set_editor.update(cx, |editor, cx| {
                                    set_from_switch(&mut editor.draft, *enabled);
                                    editor.draft_changed(cx);
                                });
                            }),
                    )
            }),
        )
    }

    pub(in crate::root) fn broadcaster_anchor_range_item(editor: Entity<Self>) -> SettingItem {
        SettingItem::new(
            "Accepted fee range",
            SettingField::<SharedString>::render(move |_options, window, cx| {
                let (min_bps, max_bps, revision) = {
                    let editor = editor.read(cx);
                    let (min_bps, max_bps) = broadcaster_anchor_bps_range(&editor.draft);
                    (min_bps, max_bps, editor.field_sync_revision)
                };
                let state = window.use_keyed_state(
                    SharedString::from("wallet-settings-broadcaster-anchor-range-slider"),
                    cx,
                    {
                        let editor = editor.clone();
                        move |_window, cx| {
                            let slider = cx.new(|_| {
                                SliderState::new()
                                    .min(ANCHOR_BPS_SLIDER_MIN)
                                    .max(ANCHOR_BPS_SLIDER_MAX)
                                    .step(ANCHOR_BPS_SLIDER_STEP)
                                    .default_value(
                                        anchor_bps_to_slider_value(min_bps)
                                            ..anchor_bps_to_slider_value(max_bps),
                                    )
                            });
                            let subscription = cx.subscribe(&slider, {
                                let editor = editor.clone();
                                move |_state: &mut SyncedAnchorRangeSliderState,
                                      _slider,
                                      event: &SliderEvent,
                                      cx| {
                                    let SliderEvent::Change(value) = event else {
                                        return;
                                    };
                                    editor.update(cx, |editor, cx| {
                                        set_broadcaster_anchor_bps_range(
                                            &mut editor.draft,
                                            value.start(),
                                            value.end(),
                                        );
                                        editor.draft_changed(cx);
                                    });
                                }
                            });
                            SyncedAnchorRangeSliderState {
                                slider,
                                synced_revision: revision,
                                _subscription: subscription,
                            }
                        }
                    },
                );

                state.update(cx, |state, cx| {
                    if state.synced_revision == revision {
                        return;
                    }
                    state.synced_revision = revision;
                    state.slider.update(cx, |slider, cx| {
                        slider.set_value(
                            anchor_bps_to_slider_value(min_bps)
                                ..anchor_bps_to_slider_value(max_bps),
                            window,
                            cx,
                        );
                    });
                });

                let slider = state.read(cx).slider.clone();
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div().flex().items_center().justify_end().child(
                            div()
                                .text_size(px(12.0))
                                .font_family(APP_MONO_FONT_FAMILY)
                                .text_color(rgb(theme::TEXT))
                                .child(format_anchor_bps_percent_range(min_bps, max_bps)),
                        ),
                    )
                    .child(Slider::new(&slider).w_full())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .text_size(px(12.0))
                            .line_height(px(16.0))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(format_anchor_premium_range(min_bps, max_bps))
                            .child(format!(
                                "Fees outside this range are marked suspicious. {}.",
                                format_anchor_bps_exact_range(min_bps, max_bps)
                            )),
                    )
            }),
        )
        .description("Transaction fees outside this percentage range are marked suspicious.")
        .layout(Axis::Vertical)
    }

    pub(in crate::root) fn number_field(
        field_id: impl Into<String>,
        editor: Entity<Self>,
        options: NumberFieldOptions,
        suffix: Option<&'static str>,
        get: impl Fn(&WalletSettings) -> f64 + 'static,
        set: impl Fn(&mut WalletSettings, f64) + 'static,
    ) -> SettingField<SharedString> {
        let field_id = field_id.into();
        let get = Rc::new(get);
        let set = Rc::new(set);
        SettingField::render(move |render_options, window, cx| {
            let (value, revision) = {
                let editor = editor.read(cx);
                (get(&editor.draft), editor.field_sync_revision)
            };
            let value_text = SharedString::from(value.to_string());
            let state = window.use_keyed_state(
                SharedString::from(format!("wallet-settings-number-{field_id}")),
                cx,
                {
                    let value_text = value_text.clone();
                    let set = set.clone();
                    let editor = editor.clone();
                    let number_options = options.clone();
                    move |window, cx| {
                        let input = cx.new(|cx| {
                            InputState::new(window, cx).default_value(value_text.clone())
                        });
                        let subscriptions = vec![
                            cx.subscribe_in(&input, window, {
                                let number_options = number_options.clone();
                                move |_, input, event: &NumberInputEvent, window, cx| {
                                    let NumberInputEvent::Step(action) = event;
                                    input.update(cx, |input, cx| {
                                        if let Ok(value) = input.value().parse::<f64>() {
                                            let new_value = match action {
                                                StepAction::Increment => {
                                                    value + number_options.step
                                                }
                                                StepAction::Decrement => {
                                                    value - number_options.step
                                                }
                                            }
                                            .clamp(number_options.min, number_options.max);
                                            input.replace_all(new_value.to_string(), window, cx);
                                        }
                                    });
                                }
                            }),
                            cx.subscribe_in(&input, window, {
                                let set = set.clone();
                                let editor = editor.clone();
                                let number_options = number_options.clone();
                                move |_state: &mut SyncedNumberFieldState,
                                      input,
                                      event: &InputEvent,
                                      window,
                                      cx| {
                                    if !matches!(event, InputEvent::Change) {
                                        return;
                                    }
                                    input.update(cx, |input, cx| {
                                        let Ok(value) = input.value().parse::<f64>() else {
                                            return;
                                        };
                                        let clamped =
                                            value.clamp(number_options.min, number_options.max);
                                        let was_clamped = value < number_options.min
                                            || value > number_options.max;
                                        editor.update(cx, |editor, cx| {
                                            set(&mut editor.draft, clamped);
                                            editor.draft_changed(cx);
                                        });
                                        if was_clamped {
                                            input.set_value(clamped.to_string(), window, cx);
                                        }
                                    });
                                }
                            }),
                        ];
                        SyncedNumberFieldState {
                            input,
                            synced_revision: revision,
                            _subscriptions: subscriptions,
                        }
                    }
                },
            );

            state.update(cx, |state, cx| {
                if state.synced_revision == revision {
                    return;
                }
                state.synced_revision = revision;
                if state.input.read(cx).value().as_ref() == value_text.as_ref() {
                    return;
                }
                state.input.update(cx, |input, cx| {
                    input.set_value(value_text.clone(), window, cx);
                });
            });

            let input = state.read(cx).input.clone();
            NumberInput::new(&input)
                .bg(rgb(theme::SURFACE))
                .with_size(render_options.size())
                .when_some(suffix, |this, suffix| this.suffix(app_muted_text(suffix)))
                .map(|this| {
                    if matches!(render_options.layout(), Axis::Horizontal) {
                        // A unit suffix takes room from the value.
                        if suffix.is_some() {
                            this.w_40()
                        } else {
                            this.w_32()
                        }
                    } else {
                        this.w_full()
                    }
                })
        })
    }

    pub(in crate::root) fn dropdown_field(
        editor: Entity<Self>,
        options: Vec<(SharedString, SharedString)>,
        get: impl Fn(&WalletSettings) -> SharedString + 'static,
        set: impl Fn(&mut WalletSettings, SharedString) + 'static,
    ) -> SettingField<SharedString> {
        let get_editor = editor.clone();
        let set_editor = editor;
        SettingField::dropdown(
            options,
            move |cx| get(&get_editor.read(cx).draft),
            move |value, cx| {
                set_editor.update(cx, |editor, cx| {
                    set(&mut editor.draft, value);
                    editor.programmatic_draft_changed(cx);
                });
            },
        )
    }
}
