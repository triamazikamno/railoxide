use super::*;

impl GatewayView {
    pub(in crate::browser) fn clear_draft_ui(&mut self) {
        self.draft = None;
        self.draft_form = None;
        self.handoff_open = false;
        self.draft_completion = None;
        self.completed_draft = None;
        self.cancel_draft_on_arrival = None;
    }

    pub(in crate::browser) fn sync_draft(
        &mut self,
        draft: Option<DraftSnapshot>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(draft) = &draft {
            if self.draft.is_none() && self.completed_draft.as_ref() == Some(&draft.id) {
                return;
            }
            if self.cancel_draft_on_arrival.as_ref() == Some(&draft.request_id) {
                send(&json!({"action":"cancel", "draft_id":draft.id}));
                return;
            }
        } else {
            // An empty snapshot can precede the Create acknowledgement. Keep a pending cancel
            // until that request arrives or the authority is retired.
            self.handoff_open = false;
            if self
                .draft_form
                .as_ref()
                .is_some_and(|form| form.id.is_some())
            {
                self.draft_form = None;
            }
        }
        self.draft = draft;
        if let Some(form) = &self.draft_form {
            let stale = if form.private.is_some() {
                form.input["wallet"].as_str() != self.private_view.selected_wallet.as_deref()
                    || json_chain_id(&form.input["chain_id"]) != self.private_view.selected_chain
            } else {
                Some(text(&form.input, "account")) != self.public_view.selected_account
                    || json_chain_id(&form.input["chain_id"]) != self.public_view.selected_chain
            };
            if stale {
                self.draft_form = None;
            }
        }
        if let Some(draft) = &self.draft {
            if let Some(form) = &mut self.draft_form {
                if form.request_id == draft.request_id {
                    form.id = Some(draft.id.clone());
                    if form.private.is_none()
                        && let Some(id) = form.input["address_book_entry"].as_str()
                        && let Some(entry) = draft.recipients.iter().find(|entry| entry.id == id)
                        && form.recipient.read(cx).value().as_ref() != entry.address
                    {
                        form.setting_recipient = Some(entry.address.clone());
                        form.recipient.update(cx, |input, cx| {
                            input.set_value(entry.address.clone(), window, cx);
                        });
                    }
                    if draft.revision == form.revision
                        && draft.input == form.input
                        && (form.input["asset"] == "native" || form.private.is_some())
                        && form.input["max"] == true
                        && let Some(amount) = draft.estimate["amount"]
                            .as_str()
                            .filter(|amount| !amount.is_empty())
                            .or_else(|| {
                                form.private.as_ref()?;
                                draft.private_options["assets"]
                                    .as_array()?
                                    .iter()
                                    .find(|asset| asset["id"] == form.input["asset"])?["max_amount"]
                                    .as_str()
                            })
                    {
                        let amount = amount.to_owned();
                        if form.amount.read(cx).value().as_ref() != amount {
                            form.setting_max = Some(amount.clone());
                            form.amount
                                .update(cx, |input, cx| input.set_value(amount, window, cx));
                        }
                    }
                    if draft.revision > form.revision {
                        // Another extension view edited this peer's draft. Reopen its latest inputs.
                        self.draft_form = None;
                    }
                } else if form.id.is_some() {
                    self.draft_form = None;
                }
            }
            if draft.executing() && !self.handoff_open {
                self.draft_form = None;
            }
            if self.handoff_open
                && !draft.executing()
                && (draft.status != "ready" || !draft.message.is_empty())
            {
                self.handoff_open = false;
            }
            if draft.status == "done"
                && (!draft.input["wallet"].is_string()
                    || draft.private_progress["result"] == "confirmed")
                && self.completed_draft.as_ref() != Some(&draft.id)
            {
                self.completed_draft = Some(draft.id.clone());
                let id = draft.id.clone();
                self.draft_completion = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let _ = this.update(cx, |this, cx| {
                        if this
                            .draft
                            .as_ref()
                            .is_some_and(|draft| draft.id == id && draft.status == "done")
                        {
                            this.dismiss_draft(cx);
                        }
                    });
                }));
            }
        }
        self.sync_draft_assets(window, cx);
        self.sync_private_draft_options(window, cx);
        self.flush_draft(cx);
    }

    pub(super) fn sync_draft_assets(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = &mut self.draft_form else {
            return;
        };
        if form.private.is_some() {
            return;
        }
        let assets = self
            .public_view
            .balances(&text(&form.input, "account"))
            .map_or_else(Vec::new, |balances| balances.assets.clone());
        if form.assets != assets {
            form.assets.clone_from(&assets);
            form.asset.update(cx, |select, cx| {
                select.set_items(FullWidthSelectItems::new(assets), window, cx);
            });
        }
        let selected = text(&form.input, "asset");
        if form.asset.read(cx).selected_value() != Some(&selected) {
            form.asset.update(cx, |select, cx| {
                select.set_selected_value(&selected, window, cx);
            });
        }
        form.sync_token_max(window, cx);
    }

    pub(in crate::browser) fn open_draft(
        &mut self,
        kind: &str,
        asset: Option<String>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.public_view.drafts_supported
            || (kind == "shield" && !self.selected_chain_has_railgun())
        {
            return;
        }
        if self.draft.as_ref().is_some_and(DraftSnapshot::executing) {
            self.handoff_open = true;
            self.sites_open = false;
            cx.notify();
            return;
        }
        let Some(account) = self.public_view.selected_account.clone() else {
            return;
        };
        let Some(chain) = self.public_view.selected_chain else {
            return;
        };
        let existing = self.draft.as_ref().filter(|draft| {
            text(&draft.input, "account") == account
                && json_chain_id(&draft.input["chain_id"]) == Some(chain)
        });
        let mut input = existing.map_or_else(
            || {
                json!({"account":account, "chain_id":chain_json(chain), "kind":kind,
            "asset":"native", "amount":"", "recipient":"", "address_book_entry":null,
            "fee":{"mode":"normal"}, "max":false})
            },
            |draft| draft.input.clone(),
        );
        defaults::select_public_mode(
            &mut input,
            kind,
            self.public_view.mimic_railway_shields_by_default,
        );
        if let Some(asset) = asset {
            input["asset"] = asset.into();
        }
        let request_id = existing.map_or_else(
            || format!("{}-{}", js_sys::Date::now(), js_sys::Math::random()),
            |draft| draft.request_id.clone(),
        );
        let id = existing.map(|draft| draft.id.clone());
        let revision = existing.map_or(0, |draft| draft.revision + 1);
        self.install_draft_form(input, request_id, id, revision, window, cx);
        self.sites_open = false;
        self.handoff_open = false;
        self.flush_draft(cx);
        cx.notify();
    }
}
