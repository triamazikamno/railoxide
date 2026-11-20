use super::*;

impl WalletSettingsEditor {
    pub(in crate::root) fn open_token_dialog(
        &self,
        target: &TokenEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let values = token_dialog_values(&self.draft, target);
        let inputs = TokenDialogInputs {
            chain_id: cx
                .new(|cx| InputState::new(window, cx).default_value(values.chain_id.to_string())),
            token_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(display_token_address(
                    values.chain_id,
                    &values.token_address,
                ))
            }),
            symbol: cx.new(|cx| InputState::new(window, cx).default_value(values.symbol.clone())),
            decimals: cx
                .new(|cx| InputState::new(window, cx).default_value(values.decimals.to_string())),
            icon_path: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(values.icon_path.clone().unwrap_or_default())
            }),
        };
        let title = match &target {
            TokenEditTarget::AddCustom => "Add custom token".to_string(),
            TokenEditTarget::BuiltIn(_) => "Edit built-in token".to_string(),
            TokenEditTarget::Custom(_) => "Edit custom token".to_string(),
        };
        let action_label = SharedString::from(if matches!(target, TokenEditTarget::AddCustom) {
            "Add"
        } else {
            "Save"
        });
        let readonly_identity = matches!(target, TokenEditTarget::BuiltIn(_));
        let dialog_width = (window.viewport_size().width * 0.92).min(px(620.0));
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let editor = cx.entity();
        let save_inputs = inputs.clone();
        let save_target = target.clone();
        let error = cx.new(|_| None::<SharedString>);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let save_editor = editor.clone();
            let save_inputs = save_inputs.clone();
            let render_inputs = save_inputs.clone();
            let save_target = save_target.clone();
            let save_error = error.clone();
            let shown_error = error.read(cx).clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(title.clone()))
                .footer(dialog_footer(action_label.clone(), true))
                .on_ok(move |_event, _window, cx| {
                    let values = match token_dialog_values_from_inputs(&save_inputs, cx) {
                        Ok(values) => values,
                        Err(message) => {
                            set_dialog_error(&save_error, message, cx);
                            return false;
                        }
                    };
                    let target = save_target.clone();
                    save_editor.update(cx, |editor, cx| {
                        apply_token_dialog_values(&mut editor.draft, &target, values);
                        editor.programmatic_draft_changed(cx);
                    });
                    true
                })
                .child(
                    render_token_dialog_content(&render_inputs, content_width, readonly_identity)
                        .when_some(shown_error, |this, error| {
                            this.child(settings_dialog_error(error))
                        }),
                )
        });
        let focus_input = if readonly_identity {
            inputs.symbol
        } else {
            inputs.chain_id
        };
        cx.defer_in(window, move |_editor, window, cx| {
            focus_input.read(cx).focus_handle(cx).focus(window, cx);
        });
    }

    pub(in crate::root) fn open_price_anchor_dialog(
        &self,
        target: &PriceAnchorEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let values = price_anchor_dialog_values(&self.draft, target);
        let chain_items = price_anchor_chain_select_items(&self.draft);
        let selected_chain_index = chain_select_index(&chain_items, values.chain_id);
        let selected_oracle_chain_index = chain_select_index(&chain_items, values.oracle_chain_id);
        let anchor_type_items = price_anchor_type_select_items();
        let selected_anchor_type_index =
            price_anchor_type_select_index(&anchor_type_items, values.anchor_type);
        let inputs = PriceAnchorDialogInputs {
            chain_id: cx
                .new(|cx| SelectState::new(chain_items.clone(), selected_chain_index, window, cx)),
            token_address: cx
                .new(|cx| InputState::new(window, cx).default_value(values.token_address.clone())),
            anchor_type: cx.new(|cx| {
                SelectState::new(anchor_type_items, selected_anchor_type_index, window, cx)
            }),
            selected_anchor_type: cx
                .new(|cx| InputState::new(window, cx).default_value(values.anchor_type)),
            fixed_rate: cx
                .new(|cx| InputState::new(window, cx).default_value(values.fixed_rate.clone())),
            oracle_chain_id: cx.new(|cx| {
                SelectState::new(chain_items.clone(), selected_oracle_chain_index, window, cx)
            }),
            oracle_address: cx
                .new(|cx| InputState::new(window, cx).default_value(values.oracle_address.clone())),
            oracle_token_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.oracle_token_decimals.clone())
            }),
            oracle_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.oracle_decimals.clone())
            }),
            oracle_is_inversed: cx.new(|cx| {
                SelectState::new(
                    bool_select_items(),
                    Some(bool_select_index(values.oracle_is_inversed)),
                    window,
                    cx,
                )
            }),
            twap_pool_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_pool_address.clone())
            }),
            twap_base_token_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_base_token_address.clone())
            }),
            twap_quote_token_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_quote_token_address.clone())
            }),
            twap_base_token_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_base_token_decimals.clone())
            }),
            twap_window_seconds: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_window_seconds.clone())
            }),
            product_scale_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.product_scale_decimals.clone())
            }),
            product_components: values
                .product_components
                .iter()
                .take(2)
                .map(|component| {
                    Self::product_anchor_component_dialog_inputs(
                        component,
                        &chain_items,
                        window,
                        cx,
                    )
                })
                .collect(),
        };
        let selected_anchor_type = inputs.selected_anchor_type.clone();
        Self::subscribe_price_anchor_type_select(
            &inputs.anchor_type,
            selected_anchor_type,
            window,
            cx,
        );
        for component in &inputs.product_components {
            Self::subscribe_price_anchor_type_select(
                &component.anchor_type,
                component.selected_anchor_type.clone(),
                window,
                cx,
            );
        }
        let viewport_size = window.viewport_size();
        let dialog_width = (viewport_size.width * 0.92).min(px(620.0));
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let editor = cx.entity();
        let save_inputs = inputs.clone();
        let save_target = target.clone();
        let title = match target {
            PriceAnchorEditTarget::Add => "Add price anchor",
            PriceAnchorEditTarget::Edit(_) => "Edit price anchor",
        };
        let action_label = SharedString::from(if matches!(target, PriceAnchorEditTarget::Add) {
            "Add"
        } else {
            "Save"
        });
        let error = cx.new(|_| None::<SharedString>);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let save_editor = editor.clone();
            let save_inputs = save_inputs.clone();
            let render_inputs = save_inputs.clone();
            let save_target = save_target.clone();
            let save_error = error.clone();
            let shown_error = error.read(cx).clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(title))
                .footer(dialog_footer(action_label.clone(), true))
                .on_ok(move |_event, _window, cx| {
                    let anchor = match price_anchor_override_from_dialog_inputs(&save_inputs, cx) {
                        Ok(anchor) => anchor,
                        Err(message) => {
                            set_dialog_error(&save_error, message, cx);
                            return false;
                        }
                    };
                    let target = save_target.clone();
                    save_editor.update(cx, |editor, cx| {
                        apply_price_anchor_dialog_values(&mut editor.draft, &target, anchor);
                        editor.programmatic_draft_changed(cx);
                    });
                    true
                })
                .child(
                    render_price_anchor_dialog_content(&render_inputs, content_width, cx)
                        .when_some(shown_error, |this, error| {
                            this.child(settings_dialog_error(error))
                        }),
                )
        });
        let focus_input = inputs.chain_id;
        cx.defer_in(window, move |_editor, window, cx| {
            focus_input.read(cx).focus_handle(cx).focus(window, cx);
        });
    }

    pub(in crate::root) fn product_anchor_component_dialog_inputs(
        values: &PriceAnchorComponentDialogValues,
        chain_items: &[NetworkSelectItem],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> ProductAnchorComponentDialogInputs {
        let component_type_items = product_component_type_select_items();
        let selected_component_type_index =
            price_anchor_type_select_index(&component_type_items, values.anchor_type);
        let selected_oracle_chain_index = chain_select_index(chain_items, values.oracle_chain_id);
        ProductAnchorComponentDialogInputs {
            anchor_type: cx.new(|cx| {
                SelectState::new(
                    component_type_items,
                    selected_component_type_index,
                    window,
                    cx,
                )
            }),
            selected_anchor_type: cx
                .new(|cx| InputState::new(window, cx).default_value(values.anchor_type)),
            fixed_rate: cx
                .new(|cx| InputState::new(window, cx).default_value(values.fixed_rate.clone())),
            oracle_chain_id: cx.new(|cx| {
                SelectState::new(
                    chain_items.to_vec(),
                    selected_oracle_chain_index,
                    window,
                    cx,
                )
            }),
            oracle_address: cx
                .new(|cx| InputState::new(window, cx).default_value(values.oracle_address.clone())),
            oracle_token_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.oracle_token_decimals.clone())
            }),
            oracle_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.oracle_decimals.clone())
            }),
            oracle_is_inversed: cx.new(|cx| {
                SelectState::new(
                    bool_select_items(),
                    Some(bool_select_index(values.oracle_is_inversed)),
                    window,
                    cx,
                )
            }),
            twap_pool_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_pool_address.clone())
            }),
            twap_base_token_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_base_token_address.clone())
            }),
            twap_quote_token_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_quote_token_address.clone())
            }),
            twap_base_token_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_base_token_decimals.clone())
            }),
            twap_window_seconds: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_window_seconds.clone())
            }),
        }
    }

    pub(in crate::root) fn subscribe_price_anchor_type_select(
        select: &Entity<SelectState<Vec<PriceAnchorTypeSelectItem>>>,
        selected_anchor_type: Entity<InputState>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        cx.subscribe_in(
            select,
            window,
            move |_editor,
                  _select,
                  event: &SelectEvent<Vec<PriceAnchorTypeSelectItem>>,
                  window,
                  cx| {
                if let SelectEvent::Confirm(Some(anchor_type)) = event {
                    selected_anchor_type.update(cx, |input, cx| {
                        input.set_value((*anchor_type).to_string(), window, cx);
                    });
                    cx.notify();
                }
            },
        )
        .detach();
    }

    pub(in crate::root) fn render_token_list(
        editor: &Entity<Self>,
        entries: Vec<DisplayTokenEntry>,
    ) -> gpui::Div {
        let add_editor = editor.clone();
        let body = div().w_full().flex().flex_col().gap_2().child(
            div().flex().child(
                app_button_base("wallet-settings-token-add")
                    .icon(IconName::Plus)
                    .outline()
                    .child(app_text("Add token"))
                    .on_click(move |_event, window, cx| {
                        add_editor.update(cx, |editor, cx| {
                            editor.open_token_dialog(&TokenEditTarget::AddCustom, window, cx);
                        });
                    }),
            ),
        );
        if entries.is_empty() {
            return body.child(app_muted_text("No tokens configured.").py(px(8.0)));
        }

        let mut list = div().w_full().flex().flex_col();
        let mut current_chain = None;
        let token_count = entries.len();
        for (index, entry) in entries.into_iter().enumerate() {
            if current_chain != Some(entry.chain_id) {
                current_chain = Some(entry.chain_id);
                list = list.child(settings_token_chain_header(entry.chain_id));
            }
            let edit_editor = editor.clone();
            let remove_editor = editor.clone();
            let edit_target = if entry.built_in {
                TokenEditTarget::BuiltIn(TokenKey {
                    chain_id: entry.chain_id,
                    token_address: entry.token_address.clone(),
                })
            } else {
                TokenEditTarget::Custom(entry.custom_index.unwrap_or(index))
            };
            let remove_index = entry.custom_index;
            let row_id = SharedString::from(format!(
                "wallet-settings-token-row-{}-{}",
                entry.chain_id, entry.token_address
            ));
            list = list.child(
                settings_list_row(row_id, index + 1 == token_count)
                    .gap_3()
                    .child(render_token_entry_summary(&entry))
                    .child(
                        settings_row_actions()
                            .child(
                                settings_icon_button(
                                    SharedString::from(format!(
                                        "wallet-settings-token-edit-{index}"
                                    )),
                                    Icon::new(RailgunActionIcon::Pencil),
                                    "Edit",
                                )
                                .on_click(
                                    move |_event, window, cx| {
                                        let target = edit_target.clone();
                                        edit_editor.update(cx, |editor, cx| {
                                            editor.open_token_dialog(&target, window, cx);
                                        });
                                    },
                                ),
                            )
                            .when(!entry.built_in, |this| {
                                this.child(
                                    settings_danger_icon_button(
                                        SharedString::from(format!(
                                            "wallet-settings-token-remove-{index}"
                                        )),
                                        Icon::new(RailgunActionIcon::Trash2),
                                        "Remove",
                                    )
                                    .on_click(
                                        move |_event, _window, cx| {
                                            if let Some(index) = remove_index {
                                                remove_editor.update(cx, |editor, cx| {
                                                    remove_custom_token(&mut editor.draft, index);
                                                    editor.programmatic_draft_changed(cx);
                                                });
                                            }
                                        },
                                    ),
                                )
                            }),
                    ),
            );
        }
        body.child(list)
    }

    pub(in crate::root) fn render_price_anchor_list(
        editor: &Entity<Self>,
        entries: Vec<DisplayPriceAnchorEntry>,
    ) -> gpui::Div {
        let add_editor = editor.clone();
        let body = div().w_full().flex().flex_col().gap_2().child(
            div().flex().child(
                app_button_base("wallet-settings-price-anchor-add")
                    .icon(IconName::Plus)
                    .outline()
                    .child(app_text("Add price anchor"))
                    .on_click(move |_event, window, cx| {
                        add_editor.update(cx, |editor, cx| {
                            editor.open_price_anchor_dialog(
                                &PriceAnchorEditTarget::Add,
                                window,
                                cx,
                            );
                        });
                    }),
            ),
        );

        if entries.is_empty() {
            return body.child(app_muted_text("No price anchors configured.").py(px(8.0)));
        }

        let mut list = div().w_full().flex().flex_col();
        let mut current_chain = None;
        let anchor_count = entries.len();
        for (index, entry) in entries.into_iter().enumerate() {
            if current_chain != Some(entry.key.chain_id) {
                current_chain = Some(entry.key.chain_id);
                list = list.child(settings_token_chain_header(entry.key.chain_id));
            }
            let edit_editor = editor.clone();
            let remove_editor = editor.clone();
            let edit_target = PriceAnchorEditTarget::Edit(entry.clone());
            let remove_entry = entry.clone();
            let can_remove = entry.override_index.is_some();
            let row_id = SharedString::from(format!(
                "wallet-settings-price-anchor-row-{}-{}",
                entry.key.chain_id, entry.key.token_address
            ));
            list = list.child(
                settings_list_row(row_id, index + 1 == anchor_count)
                    .gap_3()
                    .child(render_price_anchor_entry_summary(&entry))
                    .child(
                        settings_row_actions()
                            .child(
                                settings_icon_button(
                                    SharedString::from(format!(
                                        "wallet-settings-price-anchor-edit-{index}"
                                    )),
                                    Icon::new(RailgunActionIcon::Pencil),
                                    "Edit",
                                )
                                .on_click(
                                    move |_event, window, cx| {
                                        let target = edit_target.clone();
                                        edit_editor.update(cx, |editor, cx| {
                                            editor.open_price_anchor_dialog(&target, window, cx);
                                        });
                                    },
                                ),
                            )
                            .when(can_remove, |this| {
                                this.child(
                                    settings_danger_icon_button(
                                        SharedString::from(format!(
                                            "wallet-settings-price-anchor-remove-{index}"
                                        )),
                                        Icon::new(RailgunActionIcon::Trash2),
                                        "Remove",
                                    )
                                    .on_click(
                                        move |_event, _window, cx| {
                                            let entry = remove_entry.clone();
                                            remove_editor.update(cx, |editor, cx| {
                                                remove_display_price_anchor_override(
                                                    &mut editor.draft,
                                                    &entry,
                                                );
                                                editor.programmatic_draft_changed(cx);
                                            });
                                        },
                                    ),
                                )
                            }),
                    ),
            );
        }

        body.child(list)
    }
}
