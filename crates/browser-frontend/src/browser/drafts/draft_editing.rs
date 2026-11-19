use super::*;

impl GatewayView {
    pub(super) fn install_draft_form(
        &mut self,
        mut input: Value,
        request_id: String,
        id: Option<String>,
        revision: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Older extension views offered tiers; reopening uses the shared Auto policy.
        if matches!(input["fee"]["mode"].as_str(), Some("slow" | "fast")) {
            input["fee"] = json!({"mode":"normal"});
        }
        let amount = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("0.00")
                .default_value(text(&input, "amount"))
        });
        let recipient = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Address or ENS name")
                .default_value(text(&input, "recipient"))
        });
        // These inputs can be seeded before their first Custom-mode render. Initialize
        // their text layout with the packaged font; WASM has no system-font fallback.
        let (max_fee, priority_fee) = window.with_text_style(
            Some(gpui::TextStyleRefinement {
                font_family: Some(cx.theme().font_family.clone()),
                ..Default::default()
            }),
            |window| {
                (
                    cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder("max fee gwei")
                            .default_value(text(draft_fee(&input), "max_fee_gwei"))
                    }),
                    cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder("max tip gwei")
                            .default_value(text(draft_fee(&input), "priority_fee_gwei"))
                    }),
                )
            },
        );
        let asset = cx.new(|cx| {
            SelectState::new(
                FullWidthSelectItems::new(Vec::<AssetBalance>::new()),
                None,
                window,
                cx,
            )
            .searchable(true)
        });
        let mut subscriptions = vec![cx.subscribe_in(
            &asset,
            window,
            |this, _, event: &SelectEvent<FullWidthSelectItems<AssetBalance>>, window, cx| {
                if let SelectEvent::Confirm(Some(asset)) = event
                    && let Some(form) = &mut this.draft_form
                    && form.input["asset"].as_str() != Some(asset.as_str())
                {
                    form.input["asset"] = asset.clone().into();
                    form.sync_token_max(window, cx);
                    this.draft_changed(cx);
                }
            },
        )];
        subscriptions.push(cx.subscribe_in(
            &recipient,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let Some(form) = &this.draft_form else {
                        return;
                    };
                    if !form.recipient_open {
                        return;
                    }
                    let options = this.filtered_draft_recipients(cx);
                    if let Some(option) = form.recipient_index.and_then(|index| options.get(index))
                    {
                        this.handle_draft_recipient(
                            &RecipientPickerEvent::Select(option.id().clone()),
                            window,
                            cx,
                        );
                    }
                }
            },
        ));
        for (field, entity) in [
            ("amount", &amount),
            ("recipient", &recipient),
            ("max_fee_gwei", &max_fee),
            ("priority_fee_gwei", &priority_fee),
        ] {
            subscriptions.push(cx.subscribe(
                entity,
                move |this, entity, event: &InputEvent, cx| {
                    if !matches!(event, InputEvent::Change) {
                        return;
                    }
                    if let Some(form) = &mut this.draft_form {
                        let value = entity.read(cx).value().to_string();
                        match field {
                            "amount" => {
                                if form.setting_max.take().as_ref() == Some(&value) {
                                    return;
                                }
                                form.input["amount"] = value.into();
                                form.input["max"] = false.into();
                            }
                            "recipient" => {
                                if form.setting_recipient.take().as_ref() == Some(&value) {
                                    return;
                                }
                                form.input["recipient"] = value.into();
                                form.input["address_book_entry"] = Value::Null;
                            }
                            _ => {
                                // Seeding from a quote already updated the canonical input.
                                if draft_fee(&form.input)["mode"] != "custom"
                                    || draft_fee(&form.input)[field].as_str()
                                        == Some(value.as_str())
                                {
                                    return;
                                }
                                draft_fee_mut(&mut form.input)[field] = value.into();
                            }
                        }
                    }
                    if field == "recipient" {
                        this.update_draft_recipient_search(cx);
                    }
                    this.draft_changed(cx);
                },
            ));
        }
        let first_input = if input["kind"] == "send" {
            &recipient
        } else {
            &amount
        };
        let focus_handle = first_input.read(cx).focus_handle(cx);
        let private = input["wallet"]
            .is_string()
            .then(|| self.new_private_draft_form(&input, window, cx));
        self.draft_form = Some(DraftForm {
            private,
            request_id,
            id,
            revision,
            sent: None,
            sent_revision: None,
            input,
            assets: Vec::new(),
            asset,
            amount,
            setting_max: None,
            setting_recipient: None,
            recipient,
            recipient_open: false,
            recipient_index: None,
            recipient_scroll: gpui::ScrollHandle::new(),
            max_fee,
            priority_fee,
            _subscriptions: subscriptions,
            pending: None,
        });
        self.sync_draft_assets(window, cx);
        focus_handle.focus(window, cx);
    }

    pub(super) fn draft_gas_quote(&self) -> Option<(String, String)> {
        let form = self.draft_form.as_ref()?;
        let draft = self.draft.as_ref().filter(|draft| {
            form.id.as_ref() == Some(&draft.id)
                && form.input["wallet"] == draft.input["wallet"]
                && form.input["delivery"]["mode"] == draft.input["delivery"]["mode"]
                && ["account", "chain_id", "kind", "mimic_railway"]
                    .iter()
                    .all(|key| form.input[key] == draft.input[key])
                // A Public quote also follows its asset's fee route.
                && (form.input.get("account").is_none()
                    || form.input["asset"] == draft.input["asset"])
        })?;
        Some((
            draft.gas_quote["max_fee_gwei"].as_str()?.to_owned(),
            draft.gas_quote["priority_fee_gwei"].as_str()?.to_owned(),
        ))
    }

    pub(super) fn handle_draft_gas_fee(
        &mut self,
        event: GasFeeEditorEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let quote = self.draft_gas_quote();
        let Some(form) = &mut self.draft_form else {
            return;
        };
        let private = form.input["wallet"].is_string();
        if private && form.input["delivery"]["mode"] != "self_broadcast" {
            return;
        }
        let auto = if private { "auto" } else { "normal" };
        match event {
            GasFeeEditorEvent::Refresh => {
                if draft_fee(&form.input)["mode"] != auto {
                    return;
                }
            }
            GasFeeEditorEvent::Mode(GasFeeMode::Auto) => {
                if draft_fee(&form.input)["mode"] == auto {
                    return;
                }
                *draft_fee_mut(&mut form.input) = json!({"mode":auto});
            }
            GasFeeEditorEvent::Mode(GasFeeMode::Custom) | GasFeeEditorEvent::Edit(_) => {
                let edit = matches!(event, GasFeeEditorEvent::Edit(_));
                if !edit && draft_fee(&form.input)["mode"] == "custom" {
                    return;
                }
                if edit
                    || (form.max_fee.read(cx).value().trim().is_empty()
                        && form.priority_fee.read(cx).value().trim().is_empty())
                {
                    if let Some((max_fee, tip)) = quote {
                        form.max_fee
                            .update(cx, |input, cx| input.set_value(max_fee, window, cx));
                        form.priority_fee
                            .update(cx, |input, cx| input.set_value(tip, window, cx));
                    } else if edit {
                        return;
                    }
                }
                *draft_fee_mut(&mut form.input) = json!({
                    "mode":"custom",
                    "max_fee_gwei":form.max_fee.read(cx).value().to_string(),
                    "priority_fee_gwei":form.priority_fee.read(cx).value().to_string(),
                });
                if let GasFeeEditorEvent::Edit(target) = event {
                    let input = match target {
                        GasFeeEditTarget::MaxFee => &form.max_fee,
                        GasFeeEditTarget::MaxTip => &form.priority_fee,
                    };
                    input.read(cx).focus_handle(cx).focus(window, cx);
                }
            }
        }
        // Refresh is a new revision too: an old estimate must not enable Sign.
        self.draft_changed(cx);
    }

    pub(super) fn draft_recipient_options(&self) -> Vec<RecipientSuggestion> {
        if self
            .draft_form
            .as_ref()
            .is_some_and(|form| form.private.is_some())
        {
            return self.private_draft_recipient_options();
        }
        self.accounts
            .iter()
            .map(|account| {
                RecipientSuggestion::new(
                    format!("account:{}", account.uuid),
                    public_view::name(account),
                    account.address.clone(),
                )
                .account(true)
            })
            .chain(
                self.draft
                    .iter()
                    .flat_map(|draft| &draft.recipients)
                    .map(|entry| {
                        RecipientSuggestion::new(
                            format!("book:{}", entry.id),
                            entry.label.clone(),
                            entry.address.clone(),
                        )
                    }),
            )
            .collect()
    }

    pub(super) fn draft_recipient_query(&self, cx: &App) -> String {
        let Some(form) = &self.draft_form else {
            return String::new();
        };
        let value = form.recipient.read(cx).value().to_string();
        if (form.private.is_some()
            && self
                .draft_recipient_options()
                .iter()
                .any(|option| option.address().as_ref() == value.trim()))
            || form.input["address_book_entry"].is_string()
            || value.trim().parse::<alloy::primitives::Address>().is_ok()
        {
            String::new()
        } else {
            value
        }
    }

    fn filtered_draft_recipients(&self, cx: &App) -> Vec<RecipientSuggestion> {
        let query = self.draft_recipient_query(cx);
        self.draft_recipient_options()
            .into_iter()
            .filter(|option| option.matches(&query))
            .collect()
    }

    fn update_draft_recipient_search(&mut self, cx: &App) {
        let query = self.draft_recipient_query(cx);
        let len = self.filtered_draft_recipients(cx).len();
        if let Some(form) = &mut self.draft_form {
            form.recipient_open = !query.trim().is_empty();
            form.recipient_index = (form.recipient_open && len > 0).then_some(0);
            form.recipient_scroll.scroll_to_item(0);
        }
    }

    pub(super) fn handle_draft_recipient(
        &mut self,
        event: &RecipientPickerEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let options = self.filtered_draft_recipients(cx);
        let Some(form) = &mut self.draft_form else {
            return;
        };
        match event {
            RecipientPickerEvent::Toggle => {
                form.recipient_open = !form.recipient_open;
                form.recipient_index = (form.recipient_open && !options.is_empty()).then_some(0);
            }
            RecipientPickerEvent::Dismiss => {
                form.recipient_open = false;
                form.recipient_index = None;
            }
            RecipientPickerEvent::Move(direction) => {
                form.recipient_open = true;
                form.recipient_index =
                    suggestion_index_after_move(form.recipient_index, options.len(), *direction);
            }
            RecipientPickerEvent::Select(id) => {
                let Some(option) = options.iter().find(|option| option.id() == id) else {
                    return;
                };
                form.input["address_book_entry"] =
                    id.strip_prefix("book:").map_or(Value::Null, Into::into);
                form.input["recipient"] =
                    if form.private.is_none() && form.input["address_book_entry"].is_string() {
                        "".into()
                    } else {
                        option.address().to_string().into()
                    };
                form.setting_recipient = Some(option.address().to_string());
                form.recipient.update(cx, |input, cx| {
                    input.set_value(option.address().clone(), window, cx);
                });
                form.recipient_open = false;
                form.recipient_index = None;
                self.draft_changed(cx);
                return;
            }
        }
        if let Some(index) = form.recipient_index {
            form.recipient_scroll.scroll_to_item(index);
        }
        cx.notify();
    }

    pub(super) fn draft_changed(&mut self, cx: &mut Context<'_, Self>) {
        let Some(form) = &mut self.draft_form else {
            return;
        };
        form.revision += 1;
        // The revision changes immediately so stale estimates cannot enable Sign while typing.
        form.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let _ = this.update(cx, Self::flush_draft);
        }));
        cx.notify();
    }
    pub(super) fn flush_draft(&mut self, cx: &mut Context<'_, Self>) {
        let Some(form) = &mut self.draft_form else {
            return;
        };
        if form.input_error().is_some() {
            return;
        }
        if self.draft.as_ref().is_some_and(DraftSnapshot::executing) {
            return;
        }
        if form.sent.as_ref() == Some(&form.input) && form.sent_revision == Some(form.revision) {
            return;
        }
        if let Some(id) = &form.id {
            send(
                &json!({"action":"update", "draft_id":id, "revision":form.revision, "input":form.input}),
            );
        } else if form.sent.is_none() {
            send(&json!({"action":"create", "request_id":form.request_id, "input":form.input}));
        } else {
            return;
        }
        form.sent = Some(form.input.clone());
        form.sent_revision = Some(if form.id.is_some() { form.revision } else { 0 });
        cx.notify();
    }
    pub(super) fn submit_draft(&mut self, cx: &mut Context<'_, Self>) {
        let Some(form) = &self.draft_form else {
            return;
        };
        if form.input["delivery"]["mode"] == "self_broadcast"
            && !self.private_view.self_broadcast_supported
        {
            return;
        }
        let Some(draft) = self.draft.as_ref().filter(|draft| {
            draft.status == "ready"
                && form.id.as_ref() == Some(&draft.id)
                && form.revision == draft.revision
                && form.input == draft.input
        }) else {
            return;
        };
        send(&json!({"action":"submit", "draft_id":draft.id, "revision":draft.revision}));
        self.handoff_open = true;
        cx.notify();
    }
    pub(super) fn dismiss_draft(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(draft) = &self.draft {
            send(&json!({"action":"dismiss", "draft_id":draft.id}));
            if draft.status == "done" {
                host_command("public_view", "{\"type\":\"refresh_balances\"}");
            }
        }
        let completed = self.draft.as_ref().map(|draft| draft.id.clone());
        self.clear_draft_ui();
        self.completed_draft = completed;
        cx.notify();
    }
    pub(in crate::browser) fn hide_draft(&mut self, cx: &mut Context<'_, Self>) {
        let _ = self.retire_private_picker();
        self.handoff_open = false;
        self.draft_form = None;
        cx.notify();
    }
    pub(super) fn retry_draft(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(draft) = self
            .draft
            .as_ref()
            .filter(|draft| draft.status == "failed" && draft.can_retry)
            .cloned()
        else {
            return;
        };
        self.dismiss_draft(cx);
        let request = format!("{}-{}", js_sys::Date::now(), js_sys::Math::random());
        self.install_draft_form(draft.input, request, None, 0, window, cx);
        self.flush_draft(cx);
    }
}
