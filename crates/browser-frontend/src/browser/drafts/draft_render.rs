use super::*;

impl GatewayView {
    pub(in crate::browser) fn render_draft_banner(&self, cx: &Context<'_, Self>) -> Option<Div> {
        let draft = self.draft.as_ref()?;
        if self.handoff_open || self.draft_form.is_some() {
            return None;
        }
        let draft_id = draft.id.clone();
        let kind = text(&draft.input, "kind");
        let action = match kind.as_str() {
            "shield" => "Shield",
            "unshield" => "Unshield",
            _ => "Send",
        };
        let amount = text(&draft.estimate, "amount_label");
        let native_summary = text(&draft.private_progress, "summary");
        let summary = if !native_summary.is_empty() {
            format!("{action} {native_summary}")
        } else if amount.is_empty() {
            format!("{action} draft")
        } else {
            format!("{action} {amount}")
        };
        let attention = draft.status == "attention";
        let warning = attention && draft.warning;
        let accent = if warning {
            cx.theme().warning
        } else {
            cx.theme().primary
        };
        let icon = match draft.status.as_str() {
            "in_progress" => Spinner::new()
                .with_size(px(20.0))
                .color(cx.theme().primary)
                .into_any_element(),
            status => Icon::empty()
                .path(match status {
                    "attention" => "ui/icons/monitor.svg",
                    "done" => "icons/circle-check.svg",
                    "failed" => "icons/circle-x.svg",
                    _ => "ui/icons/pencil.svg",
                })
                .with_size(px(20.0))
                .text_color(if status == "failed" {
                    cx.theme().danger
                } else {
                    accent
                })
                .into_any_element(),
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(if attention { accent } else { cx.theme().border })
                .bg(cx.theme().background)
                .child(div().size_5().flex_none().child(icon))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            app_strong_text(if warning {
                                "Attention needed".to_owned()
                            } else if draft.executing() {
                                draft.step.clone()
                            } else {
                                format!("Continue {action} draft")
                            })
                            .whitespace_normal()
                            .when(warning, |heading| heading.text_color(accent)),
                        )
                        .child(note(summary).truncate()),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            app_button(
                                "show-draft",
                                if draft.executing() { "Show" } else { "Edit" },
                            )
                            .small()
                            .w_full()
                            .when(warning, ButtonVariants::warning)
                            .when(attention && !warning, ButtonVariants::primary)
                            .when(!attention, Button::outline)
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if matches!(kind.as_str(), "private_send" | "unshield") {
                                        this.open_private_draft(&kind, None, window, cx);
                                    } else {
                                        this.open_draft(&kind, None, window, cx);
                                    }
                                },
                            )),
                        )
                        .when(!draft.executing(), |column| {
                            column.child(
                                app_button("discard-draft", "Discard")
                                    .small()
                                    .w_full()
                                    .danger()
                                    .outline()
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        if this.draft.as_ref().is_some_and(|draft| {
                                            draft.id == draft_id && !draft.executing()
                                        }) {
                                            send(&json!({"action":"dismiss", "draft_id":draft_id}));
                                        }
                                    })),
                            )
                        }),
                ),
        )
    }

    pub(in crate::browser) fn render_draft_form(&self, cx: &Context<'_, Self>) -> Div {
        let Some(form) = &self.draft_form else {
            return div();
        };
        if form.private.is_some() {
            return self.render_private_draft_form(cx);
        }
        let shield = text(&form.input, "kind") == "shield";
        let account_id = text(&form.input, "account");
        let account = self
            .accounts
            .iter()
            .find(|account| account.uuid == account_id);
        let asset_id = text(&form.input, "asset");
        let ready = self.draft.as_ref().filter(|draft| {
            form.id.as_ref() == Some(&draft.id)
                && draft.revision == form.revision
                && draft.input == form.input
        });
        let estimate = ready
            .map(|draft| &draft.estimate)
            .filter(|estimate| estimate.is_object());
        let max_label = if asset_id == "native" {
            estimate
                .and_then(|estimate| estimate["max_amount_label"].as_str())
                .map(str::to_owned)
        } else {
            form.assets
                .iter()
                .find(|asset| asset.id == asset_id && asset.max_amount.is_some())
                .map(|asset| format!("{} {}", asset.amount, asset.symbol))
        };
        let mut fields = div()
            .id("draft-fields")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .when_some(account, |this, account| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            note(format!(
                                "From {} · {}",
                                public_view::name(account),
                                short_address(&account.address)
                            ))
                            .flex_1()
                            .min_w_0()
                            .truncate(),
                        )
                        .child(
                            note(public_view::chain_name(
                                &self.chains,
                                json_chain_id(&form.input["chain_id"]).unwrap_or_default(),
                            ))
                            .flex_none(),
                        ),
                )
            })
            .child(public_action_mode_group(
                "draft-mode",
                shield,
                false,
                cx.listener(|this, shield, _, cx| {
                    if let Some(form) = &mut this.draft_form {
                        defaults::select_public_mode(
                            &mut form.input,
                            if *shield { "shield" } else { "send" },
                            this.public_view.mimic_railway_shields_by_default,
                        );
                        this.draft_changed(cx);
                    }
                }),
            ))
            .child(
                div().flex().flex_col().gap_1().child(note("Asset")).child(
                    Select::new(&form.asset)
                        .w_full()
                        .accessibility_label("Asset")
                        .placeholder("Choose asset")
                        .search_placeholder("Search assets"),
                ),
            )
            .when(!shield, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(note("To"))
                        .child(
                            RecipientPicker::new(
                                "draft-recipient",
                                &form.recipient,
                                self.draft_recipient_options(),
                                cx.listener(Self::handle_draft_recipient),
                            )
                            .query(self.draft_recipient_query(cx))
                            .suggestions(
                                form.recipient_open,
                                form.recipient_index,
                                &form.recipient_scroll,
                            ),
                        )
                        .when_some(
                            estimate
                                .filter(|_| !self.draft_recipient_query(cx).trim().is_empty())
                                .and_then(|estimate| estimate["recipient"].as_str()),
                            |this, address| {
                                this.child(
                                    note(address.to_owned())
                                        .font_family(MONO)
                                        .whitespace_normal(),
                                )
                            },
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(note("Amount"))
                            .child(amount_max_button("draft-max", max_label).on_click(
                                cx.listener(|this, _, window, cx| {
                                    if let Some(form) = &mut this.draft_form {
                                        form.input["max"] = true.into();
                                        form.input["amount"] = "".into();
                                        form.sync_token_max(window, cx);
                                    }
                                    this.draft_changed(cx);
                                }),
                            )),
                    )
                    .child(
                        app_input(&form.amount)
                            .large()
                            .w_full()
                            .aria_label("Amount"),
                    )
                    .child(
                        note(
                            estimate
                                .and_then(|value| value["amount_value"].as_str())
                                // Preserve the text row while its estimate is unavailable.
                                .unwrap_or("\u{a0}")
                                .to_owned(),
                        )
                        .text_right()
                        .flex_none(),
                    ),
            );
        if shield {
            fields = fields
                .child(
                    div().flex().items_start().gap_2().child(
                        Switch::new("draft-mimic")
                            .small()
                            .checked(form.input["mimic_railway"] == true)
                            .label("Mimic Railway")
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                if let Some(form) = &mut this.draft_form {
                                    form.input["mimic_railway"] = (*checked).into();
                                }
                                this.draft_changed(cx);
                            })),
                    ),
                )
                .child(
                    note("Signs this shield with the fee, gas and approval policy Railway v5.26.1 uses for a single-asset shield. The shield transaction itself is the same one Railoxide sends.")
                        .whitespace_normal(),
                );
        }
        fields = fields.child(
            GasFeeEditor::new(
                "draft-gas",
                &form.max_fee,
                &form.priority_fee,
                cx.listener(|this, event: &GasFeeEditorEvent, window, cx| {
                    this.handle_draft_gas_fee(*event, window, cx);
                }),
            )
            .mode(if form.input["fee"]["mode"] == "custom" {
                GasFeeMode::Custom
            } else {
                GasFeeMode::Auto
            })
            .quote(self.draft_gas_quote())
            .refreshing(ready.is_none_or(|draft| draft.status == "estimating")),
        );
        if let Some(estimate) = estimate {
            fields = fields.child(estimated_fees(
                estimate["gas_limit"].as_str().map(str::to_owned),
                text(estimate, "expected_gas_cost"),
                (estimate["show_maximum_gas_cost"] != false)
                    .then(|| text(estimate, "maximum_gas_cost")),
                estimate["protocol_fee"].as_str().map(|fee| {
                    (
                        estimate["protocol_fee_label"]
                            .as_str()
                            .unwrap_or("Protocol fee")
                            .to_owned(),
                        fee.to_owned(),
                    )
                }),
                MONO,
            ));
        }
        if let Some(draft) = ready
            && !draft.message.is_empty()
            && !draft.executing()
        {
            fields = fields.child(note(draft.message.clone()).whitespace_normal());
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(Self::back_title(if shield { "Shield" } else { "Send" }, cx))
            .child(fields)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap_2()
                    .pt_2()
                    .child(
                        app_button("cancel-draft", "Cancel")
                            .outline()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(form) = &this.draft_form {
                                    if let Some(id) = &form.id {
                                        send(&json!({"action":"cancel", "draft_id":id}));
                                    } else {
                                        this.cancel_draft_on_arrival =
                                            Some(form.request_id.clone());
                                    }
                                }
                                this.hide_draft(cx);
                            })),
                    )
                    .child(
                        desktop_action_button("sign-draft", "Sign in desktop app")
                            .primary()
                            .flex_1()
                            .disabled(
                                estimate.is_none()
                                    || ready.is_none_or(|draft| draft.status != "ready"),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.submit_draft(cx))),
                    ),
            )
    }

    pub(in crate::browser) fn render_draft_handoff(&self, cx: &Context<'_, Self>) -> Div {
        let Some(draft) = &self.draft else {
            return div().child(shimmer_note("Waiting for the desktop app…"));
        };
        if draft.input["wallet"].is_string() {
            return Self::render_private_draft_handoff(draft, cx);
        }
        let terminal = matches!(draft.status.as_str(), "done" | "failed");
        let id = draft.id.clone();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .gap_4()
            .child(
                app_strong_text(if draft.step.is_empty() {
                    "Opening desktop approval".to_owned()
                } else {
                    draft.step.clone()
                })
                .text_xl()
                .whitespace_normal(),
            )
            .child(if draft.status == "attention" && draft.warning {
                Alert::warning("draft-attention", draft.message.clone())
                    .small()
                    .into_any_element()
            } else {
                note(draft.message.clone())
                    .whitespace_normal()
                    .into_any_element()
            })
            .when(draft.status == "in_progress", |this| {
                this.child(
                    div()
                        .id("draft-working")
                        .flex_none()
                        .flex()
                        .justify_center()
                        .child(Spinner::new().with_size(px(48.0)).color(cx.theme().primary)),
                )
            })
            .when(!terminal, |this| this.child(summon_desktop_button()))
            .child(
                div()
                    .mt_auto()
                    .pt_4()
                    .flex()
                    .gap_2()
                    .child(
                        app_button("hide-handoff", if terminal { "Dismiss" } else { "Hide" })
                            .outline()
                            .flex_1()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if terminal {
                                    this.dismiss_draft(cx);
                                } else {
                                    this.hide_draft(cx);
                                }
                            })),
                    )
                    .when(draft.can_cancel && !terminal, |this| {
                        this.child(app_button("stop-draft", "Cancel").outline().on_click(
                            move |_, _, _| send(&json!({"action":"cancel", "draft_id":id})),
                        ))
                    })
                    .when(draft.status == "done", |this| {
                        this.child(
                            app_button("done-draft", "Done")
                                .primary()
                                .flex_1()
                                .on_click(cx.listener(|this, _, _, cx| this.dismiss_draft(cx))),
                        )
                    })
                    .when(draft.status == "failed" && draft.can_retry, |this| {
                        this.child(
                            app_button("retry-draft", "Edit and retry")
                                .primary()
                                .flex_1()
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.retry_draft(window, cx)),
                                ),
                        )
                    }),
            )
    }
}
