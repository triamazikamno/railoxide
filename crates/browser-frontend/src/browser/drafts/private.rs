//! Private form composition. Eligibility, exact amounts and estimates come from the desktop.
mod self_broadcast;
use super::*;
use gpui_component::{Selectable as _, tooltip::Tooltip};
use std::collections::{BTreeMap, BTreeSet};
use ui::broadcaster_picker::{
    BroadcasterPickerEntry, BroadcasterPickerGroupKey, BroadcasterPickerLayout,
    BroadcasterPickerRow, BroadcasterPickerSelectedCollapse, BroadcasterPickerViewMode,
    project_broadcaster_picker_entries, update_broadcaster_picker_group_expansion,
};
use ui::private_action::{BroadcasterSettings, BroadcasterSettingsEvent, DisplayRow};

#[derive(Clone, PartialEq, Eq)]
struct FeeChoice {
    asset: AssetBalance,
    count: Option<usize>,
    spendable: Option<String>,
    max_amount_label: Option<String>,
}
impl SelectItem for FeeChoice {
    type Value = String;
    fn title(&self) -> SharedString {
        self.asset.symbol.clone().into()
    }
    fn value(&self) -> &String {
        &self.asset.id
    }
    fn display_title(&self) -> Option<AnyElement> {
        Some(self.row().into_any_element())
    }
    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.row()
    }
}
impl FeeChoice {
    fn row(&self) -> Div {
        let icon = (!self.asset.icon.is_empty()).then(|| self.asset.icon.clone().into());
        match self.count {
            Some(count) => ui::private_action::fee_token_row(&self.asset.symbol, icon, count),
            None => ui::private_action::asset_row(self.asset.symbol.clone(), icon).when_some(
                self.spendable.as_ref(),
                |row, amount| {
                    let label = format!("{amount} spendable");
                    row.child(
                        app_button_label(label.clone())
                            .text_color(rgb(ui::theme::TEXT_MUTED))
                            .id(SharedString::from(format!(
                                "private-spendable-{}",
                                self.asset.id
                            )))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .tooltip(move |window, cx| {
                                Tooltip::new(label.clone()).build(window, cx)
                            }),
                    )
                },
            ),
        }
    }
}

pub(super) struct PrivateDraftForm {
    self_broadcast: self_broadcast::SelfBroadcastForm,
    asset: Entity<SelectState<SearchableVec<FeeChoice>>>,
    asset_choices: Vec<FeeChoice>,
    _asset_subscription: Subscription,
    fee: Entity<SelectState<SearchableVec<FeeChoice>>>,
    fee_choices: Vec<FeeChoice>,
    _fee_subscription: Subscription,
    display: Option<PrivateDraftDisplay>,
    breakdown_open: bool,
    picker: Option<PrivatePicker>,
}

// Presentation only. Submission still requires the current draft revision and inputs.
struct PrivateDraftDisplay {
    input: Value,
    options: Value,
    estimate: Value,
}
impl PrivateDraftDisplay {
    fn matches(&self, input: &Value) -> bool {
        self.input
            .as_object()
            .zip(input.as_object())
            .is_some_and(|(previous, next)| {
                previous.len() == next.len()
                    && previous.iter().all(|(key, value)| {
                        matches!(key.as_str(), "fee_mode" | "unwrap" | "native_top_up")
                            || next.get(key) == Some(value)
                    })
            })
    }
}

pub(super) struct PrivatePicker {
    id: String,
    query: Entity<InputState>,
    _subscription: Subscription,
    return_focus: Option<gpui::FocusHandle>,
    rows: Vec<BroadcasterPickerRow>,
    expanded: BTreeSet<BroadcasterPickerGroupKey>,
    collapsed: BTreeMap<BroadcasterPickerGroupKey, BroadcasterPickerSelectedCollapse>,
    scroll: gpui::ScrollHandle,
    sent: Option<(u64, String)>,
    total: usize,
    popover_open: bool,
}

fn asset(value: &Value) -> AssetBalance {
    AssetBalance {
        id: text(value, "id"),
        symbol: text(value, "label"),
        amount: text(value, "available"),
        max_amount: value["max_amount"].as_str().map(str::to_owned),
        usd: String::new(),
        icon: value["icon"]
            .as_str()
            .filter(|path| path.starts_with("railgun-ui/"))
            .unwrap_or_default()
            .into(),
    }
}
fn asset_choices_from_options(options: &Value) -> Option<Vec<FeeChoice>> {
    Some(
        options["assets"]
            .as_array()?
            .iter()
            .map(|value| FeeChoice {
                asset: asset(value),
                count: None,
                spendable: Some(text(value, "available")),
                max_amount_label: value["max_amount_label"].as_str().map(str::to_owned),
            })
            .collect(),
    )
}
fn rows(value: &Value) -> Vec<DisplayRow> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| DisplayRow {
            label: text(row, "label"),
            value: text(row, "value"),
            suffix: row["suffix"].as_str().map(str::to_owned),
        })
        .collect()
}

impl GatewayView {
    pub(in crate::browser) fn open_private_draft(
        &mut self,
        kind: &str,
        asset: Option<String>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.private_view.actions_supported {
            return;
        }
        if self.draft.as_ref().is_some_and(DraftSnapshot::executing) {
            self.handoff_open = true;
            self.sites_open = false;
            cx.notify();
            return;
        }
        let (Some(wallet), Some(chain)) = (
            self.private_view.selected_wallet.clone(),
            self.private_view.selected_chain,
        ) else {
            return;
        };
        let existing = self
            .draft
            .as_ref()
            .filter(|draft| draft.input["wallet"] == wallet && draft.input["chain_id"] == chain);
        let default_asset = asset.clone().unwrap_or_else(|| {
            self.private_view
                .draft_assets()
                .first()
                .map_or_else(String::new, |asset| asset.id.clone())
        });
        let mut input = existing.map_or_else(|| json!({
            "wallet":wallet,"chain_id":chain_json(chain),"kind":kind,"asset":default_asset,"amount":"","max":false,
            "recipient":"","address_book_entry":null,"fee_token":default_asset,"fee_mode":"deduct",
            "broadcaster":{"mode":"random"},"allow_out_of_range":false,"favorites_only":false
        }), |draft| draft.input.clone());
        if input["kind"] != kind {
            input["recipient"] = "".into();
            input["address_book_entry"] = Value::Null;
        }
        let selected_asset = asset.unwrap_or_else(|| text(&input, "asset"));
        defaults::select_private_output(
            &mut input,
            kind,
            &selected_asset,
            self.private_view.default_unwrap_asset.as_deref(),
        );
        let request = existing.map_or_else(
            || format!("{}-{}", js_sys::Date::now(), js_sys::Math::random()),
            |draft| draft.request_id.clone(),
        );
        let id = existing.map(|draft| draft.id.clone());
        let revision = existing.map_or(0, |draft| draft.revision + 1);
        self.install_draft_form(input, request, id, revision, window, cx);
        self.sites_open = false;
        self.handoff_open = false;
        self.flush_draft(cx);
        cx.notify();
    }

    pub(super) fn new_private_draft_form(
        &self,
        input: &Value,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> PrivateDraftForm {
        let asset_choices: Vec<_> = self
            .private_view
            .draft_assets()
            .into_iter()
            .map(|asset| FeeChoice {
                asset,
                count: None,
                spendable: None,
                max_amount_label: None,
            })
            .collect();
        let asset = cx.new(|cx| {
            SelectState::new(SearchableVec::new(asset_choices.clone()), None, window, cx)
                .searchable(true)
        });
        let selected = text(input, "asset");
        asset.update(cx, |state, cx| {
            state.set_selected_value(&selected, window, cx);
        });
        let asset_subscription = cx.subscribe(
            &asset,
            |this, _, event: &SelectEvent<SearchableVec<FeeChoice>>, cx| {
                if let SelectEvent::Confirm(Some(id)) = event
                    && let Some(form) = &mut this.draft_form
                    && form.input["asset"] != *id
                {
                    let kind = text(&form.input, "kind");
                    defaults::select_private_output(
                        &mut form.input,
                        &kind,
                        id,
                        this.private_view.default_unwrap_asset.as_deref(),
                    );
                    this.draft_changed(cx);
                }
            },
        );
        let fee = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(Vec::<FeeChoice>::new()),
                None,
                window,
                cx,
            )
            .searchable(true)
        });
        let subscription = cx.subscribe(
            &fee,
            |this, _, event: &SelectEvent<SearchableVec<FeeChoice>>, cx| {
                if let SelectEvent::Confirm(Some(id)) = event
                    && let Some(form) = &mut this.draft_form
                    && form.input["fee_token"] != *id
                {
                    form.input["fee_token"] = id.clone().into();
                    this.draft_changed(cx);
                }
            },
        );
        PrivateDraftForm {
            self_broadcast: self_broadcast::SelfBroadcastForm::new(input, window, cx),
            asset,
            asset_choices,
            _asset_subscription: asset_subscription,
            fee,
            fee_choices: Vec::new(),
            _fee_subscription: subscription,
            display: None,
            breakdown_open: false,
            picker: None,
        }
    }

    pub(super) fn sync_private_draft_options(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = &mut self.draft_form else {
            return;
        };
        let Some(private) = &mut form.private else {
            return;
        };
        let Some(draft) = self.draft.as_ref().filter(|draft| {
            form.id.as_ref() == Some(&draft.id)
                && form.revision == draft.revision
                && form.input == draft.input
        }) else {
            return;
        };
        let estimate = if draft.status == "estimating" && !draft.estimate.is_object() {
            private
                .display
                .as_ref()
                .filter(|display| display.matches(&draft.input))
                .map_or(Value::Null, |display| display.estimate.clone())
        } else {
            draft.estimate.clone()
        };
        private.display = Some(PrivateDraftDisplay {
            input: draft.input.clone(),
            options: draft.private_options.clone(),
            estimate,
        });
        private.self_broadcast.sync(
            &form.input,
            &draft.private_options["self_broadcast"],
            window,
            cx,
        );
        let asset_choices =
            asset_choices_from_options(&draft.private_options).unwrap_or_else(|| {
                self.private_view
                    .draft_assets()
                    .into_iter()
                    .map(|asset| FeeChoice {
                        asset,
                        count: None,
                        spendable: None,
                        max_amount_label: None,
                    })
                    .collect()
            });
        if private.asset_choices != asset_choices {
            private.asset_choices.clone_from(&asset_choices);
            private.asset.update(cx, |state, cx| {
                state.set_items(SearchableVec::new(asset_choices), window, cx);
            });
        }
        let selected_asset = text(&form.input, "asset");
        if private.asset.read(cx).selected_value() != Some(&selected_asset) {
            private.asset.update(cx, |state, cx| {
                state.set_selected_value(&selected_asset, window, cx);
            });
        }
        let choices: Vec<_> = draft.private_options["fee_tokens"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|value| FeeChoice {
                asset: asset(value),
                count: Some(value["broadcaster_count"].as_u64().unwrap_or_default() as usize),
                spendable: None,
                max_amount_label: None,
            })
            .collect();
        if choices != private.fee_choices {
            private.fee_choices.clone_from(&choices);
            private.fee.update(cx, |state, cx| {
                state.set_items(SearchableVec::new(choices), window, cx);
            });
        }
        let selected = text(&form.input, "fee_token");
        if private.fee.read(cx).selected_value() != Some(&selected) {
            private.fee.update(cx, |state, cx| {
                state.set_selected_value(&selected, window, cx);
            });
        }
        if let Some(picker) = &mut private.picker {
            let projection = &draft.private_options["picker"];
            let query = picker.query.read(cx).value().trim().to_ascii_lowercase();
            if projection["view_id"] == picker.id && projection["query"] == query {
                picker.rows =
                    serde_json::from_value(projection["rows"].clone()).unwrap_or_default();
                picker.total = projection["total_count"].as_u64().unwrap_or_default() as usize;
            }
        }
        self.flush_private_picker(cx);
    }

    pub(super) fn private_draft_recipient_options(&self) -> Vec<RecipientSuggestion> {
        self.draft
            .iter()
            .flat_map(|draft| &draft.recipients)
            .map(|entry| {
                let id = if entry.id.is_empty() {
                    format!("native:{}", entry.address)
                } else {
                    format!("book:{}", entry.id)
                };
                RecipientSuggestion::new(id, entry.label.clone(), entry.address.clone())
                    .account(entry.id.is_empty())
            })
            .collect()
    }

    fn switch_private_mode(&mut self, send: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        let kind = if send { "private_send" } else { "unshield" };
        let recipient = String::new();
        let Some(form) = &mut self.draft_form else {
            return;
        };
        if form.input["kind"] == kind {
            return;
        }
        let asset = text(&form.input, "asset");
        defaults::select_private_output(
            &mut form.input,
            kind,
            &asset,
            self.private_view.default_unwrap_asset.as_deref(),
        );
        form.input["recipient"] = recipient.clone().into();
        form.input["address_book_entry"] = Value::Null;
        form.setting_recipient = Some(recipient.clone());
        form.recipient
            .update(cx, |state, cx| state.set_value(recipient, window, cx));
        form.recipient_open = false;
        self.draft_changed(cx);
    }

    fn private_settings(
        &mut self,
        event: BroadcasterSettingsEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if matches!(event, BroadcasterSettingsEvent::ChooseSpecific) {
            self.open_private_picker(window, cx);
            return;
        }
        let Some(form) = &mut self.draft_form else {
            return;
        };
        match event {
            BroadcasterSettingsEvent::AllowOutOfRange(value) => {
                form.input["allow_out_of_range"] = value.into();
            }
            BroadcasterSettingsEvent::FavoritesOnly(value) => {
                form.input["favorites_only"] = value.into();
            }
            BroadcasterSettingsEvent::Random => {
                form.input["broadcaster"] = json!({"mode":"random"});
            }
            BroadcasterSettingsEvent::ChooseSpecific => return,
        }
        self.draft_changed(cx);
    }

    fn open_private_picker(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search broadcasters"));
        let subscription = cx.subscribe(&query, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.flush_private_picker(cx);
                cx.notify();
            }
        });
        let Some(private) = self
            .draft_form
            .as_mut()
            .and_then(|form| form.private.as_mut())
        else {
            return;
        };
        private.picker = Some(PrivatePicker {
            id: format!("{}-{}", js_sys::Date::now(), js_sys::Math::random()),
            query: query.clone(),
            _subscription: subscription,
            return_focus: window.focused(cx),
            rows: Vec::new(),
            expanded: BTreeSet::new(),
            collapsed: BTreeMap::new(),
            scroll: gpui::ScrollHandle::new(),
            sent: None,
            total: 0,
            popover_open: false,
        });
        query.read(cx).focus_handle(cx).focus(window, cx);
        self.flush_private_picker(cx);
        cx.notify();
    }

    fn flush_private_picker(&mut self, cx: &Context<'_, Self>) {
        let Some(form) = &mut self.draft_form else {
            return;
        };
        let Some(picker) = form
            .private
            .as_mut()
            .and_then(|private| private.picker.as_mut())
        else {
            return;
        };
        let Some(draft) = self.draft.as_ref().filter(|draft| {
            form.id.as_ref() == Some(&draft.id)
                && draft.revision == form.revision
                && draft.input == form.input
        }) else {
            return;
        };
        let query = picker.query.read(cx).value().trim().to_ascii_lowercase();
        if picker.sent.as_ref() == Some(&(form.revision, query.clone())) {
            return;
        }
        send(
            &json!({"action":"private_picker","draft_id":draft.id,"revision":form.revision,"view_id":picker.id,"open":true,"query":query}),
        );
        picker.sent = Some((form.revision, query));
    }

    pub(super) fn retire_private_picker(&mut self) -> Option<PrivatePicker> {
        let form = self.draft_form.as_mut()?;
        let picker = form.private.as_mut()?.picker.take()?;
        if let Some(id) = &form.id {
            send(
                &json!({"action":"private_picker","draft_id":id,"revision":form.revision,"view_id":picker.id,"open":false,"query":""}),
            );
        }
        Some(picker)
    }

    pub(in crate::browser) fn close_private_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(picker) = self.retire_private_picker() else {
            return false;
        };
        if let Some(focus) = picker.return_focus {
            focus.focus(window, cx);
        }
        cx.notify();
        true
    }

    pub(super) fn render_private_draft_form(&self, cx: &Context<'_, Self>) -> Div {
        let Some(form) = &self.draft_form else {
            return div();
        };
        let Some(private) = &form.private else {
            return div();
        };
        if let Some(picker) = &private.picker {
            return Self::render_private_picker(picker, cx);
        }
        let current = self.draft.as_ref().filter(|draft| {
            form.id.as_ref() == Some(&draft.id)
                && draft.revision == form.revision
                && draft.input == form.input
        });
        let empty = Value::Null;
        // Keep the previous display through refresh and fee/output option changes, including
        // the debounce before the desktop acknowledges the new revision.
        let display = private
            .display
            .as_ref()
            .filter(|display| display.matches(&form.input));
        let options = private
            .display
            .as_ref()
            .filter(|display| {
                ["wallet", "chain_id", "kind", "asset"]
                    .iter()
                    .all(|key| display.input[key] == form.input[key])
            })
            .map_or(&empty, |display| &display.options);
        let estimate = display.map_or(&empty, |display| &display.estimate);
        let selected_asset = private
            .asset_choices
            .iter()
            .find(|choice| choice.asset.id == form.input["asset"]);
        let max_amount = estimate["max_amount"]
            .as_str()
            .filter(|amount| !amount.is_empty())
            .or_else(|| selected_asset.and_then(|choice| choice.asset.max_amount.as_deref()))
            .map(str::to_owned);
        let render_warnings = |top_up| {
            options["warnings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .enumerate()
                .filter(move |(_, warning)| {
                    (*warning == ui::private_action::NATIVE_TOP_UP_LINKAGE_WARNING) == top_up
                })
                .map(|(index, warning)| {
                    ui::private_action::warning(
                        SharedString::from(format!("private-warning-{index}")),
                        warning.to_owned(),
                    )
                })
        };
        let send_mode = form.input["kind"] == "private_send";
        let self_broadcast = form.input["delivery"]["mode"] == "self_broadcast";
        let ready = current.is_some_and(|draft| draft.status == "ready")
            && (!self_broadcast || self.private_view.self_broadcast_supported);
        let mut fields = div()
            .id("private-draft-fields")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .child(ui::controls::action_mode_group(
                "private-mode",
                send_mode,
                false,
                [
                    ("send", "Send", "ui/icons/arrow-big-right-dash.svg"),
                    ("unshield", "Unshield", "ui/icons/shield.svg"),
                ],
                cx.listener(|this, send: &bool, window, cx| {
                    this.switch_private_mode(*send, window, cx);
                }),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(note("Asset"))
                    .child(ui::private_action::asset_select(&private.asset, false)),
            )
            .child(
                div().flex().flex_col().gap_1().child(note("To")).child(
                    RecipientPicker::new(
                        "private-recipient",
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
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .gap_2()
                            .child(note("Amount"))
                            .child(
                                amount_max_button(
                                    "private-max",
                                    estimate["max_amount_label"]
                                        .as_str()
                                        .map(str::to_owned)
                                        .or_else(|| {
                                            selected_asset
                                                .and_then(|choice| choice.max_amount_label.clone())
                                        }),
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        if let Some(form) = &mut this.draft_form {
                                            form.input["max"] = true.into();
                                            form.input["amount"] = "".into();
                                            if let Some(amount) = &max_amount {
                                                form.setting_max = Some(amount.clone());
                                                form.amount.update(cx, |input, cx| {
                                                    input.set_value(amount.clone(), window, cx);
                                                });
                                            }
                                        }
                                        this.draft_changed(cx);
                                    },
                                )),
                            ),
                    )
                    .child(ui::private_action::amount_input(
                        &form.amount,
                        false,
                        ready,
                        {
                            let root = cx.entity();
                            move |_, cx| root.update(cx, Self::submit_draft)
                        },
                    )),
            );
        if let Some(labels) = options["unwrap_labels"]
            .as_array()
            .filter(|labels| labels.len() == 2)
        {
            let root = cx.entity();
            fields = fields.child(
                ui::private_action::unshield_output_toggle(
                    "private-output",
                    labels[0].as_str().unwrap_or_default().to_owned(),
                    labels[1].as_str().unwrap_or_default().to_owned(),
                    form.input["unwrap"] == true,
                    false,
                    move |unwrap, _, cx| {
                        root.update(cx, |root, cx| {
                            if let Some(form) = &mut root.draft_form {
                                form.input["unwrap"] = unwrap.into();
                                form.input["native_top_up"] = false.into();
                            }
                            root.draft_changed(cx);
                        });
                    },
                )
                .flex_row()
                .items_center()
                .justify_between()
                .gap_3(),
            );
        }
        if options["native_top_up"].is_object() || form.input["native_top_up"] == true {
            fields = fields.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(ui::private_action::native_top_up_control(
                        "private-top-up",
                        options["native_top_up"]["label"]
                            .as_str()
                            .unwrap_or("Native gas top-up")
                            .to_owned(),
                        options["native_top_up"]["funding_detail"]
                            .as_str()
                            .unwrap_or("Unavailable for the current inputs. Turn off to continue.")
                            .to_owned(),
                        form.input["native_top_up"] == true,
                        false,
                        cx.listener(|this, enabled: &bool, _, cx| {
                            if let Some(form) = &mut this.draft_form {
                                form.input["native_top_up"] = (*enabled).into();
                            }
                            this.draft_changed(cx);
                        }),
                    ))
                    .children(render_warnings(true)),
            );
        }
        if self.private_view.self_broadcast_supported || self_broadcast {
            use ui::private_action::self_broadcast::{DeliveryChoice, delivery_selector};
            let root = cx.entity();
            fields = fields.child(delivery_selector(
                "private-delivery",
                if self_broadcast {
                    DeliveryChoice::SelfBroadcast
                } else {
                    DeliveryChoice::Broadcaster
                },
                self.private_view.self_broadcast_supported && options["self_broadcast"].is_object(),
                false,
                options["self_broadcast"]["show_sponsorship"] != true,
                false,
                move |choice, _, cx| {
                    root.update(cx, |root, cx| root.switch_private_delivery(choice, cx));
                },
            ));
        }
        if self_broadcast {
            fields = fields.child(self.render_private_self_broadcast(options, current, cx));
            if !self.private_view.self_broadcast_supported {
                fields = fields.child(note("Self-broadcast is unavailable in this desktop version. Choose Public broadcaster to continue.").whitespace_normal());
            }
        } else {
            fields = fields.child(ui::private_action::broadcaster_settings(
                "private-broadcaster-settings",
                BroadcasterSettings {
                    allow_out_of_range: form.input["allow_out_of_range"] == true,
                    favorites_only: form.input["favorites_only"] == true,
                    random_selected: form.input["broadcaster"]["mode"] == "random",
                    specific_label: options["specific_label"]
                        .as_str()
                        .unwrap_or("Choose specific…")
                        .into(),
                    candidate_count: options["candidate_count"].as_u64().unwrap_or_default()
                        as usize,
                    disabled: false,
                },
                ui::private_action::fee_token_control(
                    Select::new(&private.fee).disabled(private.fee_choices.is_empty()),
                ),
                None,
                {
                    let root = cx.entity();
                    move |event, window, cx| {
                        root.update(cx, |root, cx| root.private_settings(event, window, cx));
                    }
                },
            ));
        }
        if options["show_fee_mode"] == true {
            let root = cx.entity();
            fields = fields.child(ui::private_action::fee_mode_toggle(
                "private-fee-mode",
                !send_mode,
                !self_broadcast,
                form.input["fee_mode"] == "add_on_top",
                false,
                move |add, _, cx| {
                    root.update(cx, |root, cx| {
                        if let Some(form) = &mut root.draft_form {
                            form.input["fee_mode"] =
                                if add { "add_on_top" } else { "deduct" }.into();
                        }
                        root.draft_changed(cx);
                    });
                },
            ));
        }
        fields = fields.children(render_warnings(false));
        if self_broadcast {
            if let Ok(display) = serde_json::from_value::<
                ui::private_action::self_broadcast::FeeDisplay,
            >(estimate["self_broadcast_fees"].clone())
            {
                let root = cx.entity();
                fields = fields.child(ui::private_action::self_broadcast::estimated_fees(
                    "private-self-fees",
                    &display,
                    text(estimate, "protocol_fee_label"),
                    private.breakdown_open,
                    move |open, _, cx| {
                        root.update(cx, |root, cx| {
                            if let Some(private) = root
                                .draft_form
                                .as_mut()
                                .and_then(|form| form.private.as_mut())
                            {
                                private.breakdown_open = open;
                            }
                            cx.notify();
                        });
                    },
                ));
            }
        } else if estimate.is_object() {
            fields = fields
                .child(ui::private_action::estimated_outcome(
                    text(estimate, "broadcaster"),
                    rows(&estimate["outcome"]),
                    ui::private_action::transaction_fee_breakdown(
                        "private-fee-breakdown",
                        text(estimate, "transaction_fee"),
                        rows(&estimate["fee_breakdown"]),
                        text(estimate, "network_gas"),
                        private.breakdown_open,
                        None,
                        {
                            let root = cx.entity();
                            move |open, _, cx| {
                                root.update(cx, |root, cx| {
                                    if let Some(private) = root
                                        .draft_form
                                        .as_mut()
                                        .and_then(|form| form.private.as_mut())
                                    {
                                        private.breakdown_open = open;
                                    }
                                    cx.notify();
                                });
                            }
                        },
                    ),
                    text(estimate, "shape"),
                    "private-refresh-estimate",
                    current.is_none_or(|draft| draft.status == "estimating"),
                    {
                        let root = cx.entity();
                        move |_, cx| root.update(cx, Self::draft_changed)
                    },
                ))
                .children(
                    estimate["warnings"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(|warning| note(warning.to_owned()).whitespace_normal()),
                );
        }
        if let Some(draft) = current
            && draft.status == "ready"
            && !draft.message.is_empty()
        {
            fields = fields.child(note(draft.message.clone()).whitespace_normal());
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(Self::back_title(
                if send_mode { "Send" } else { "Unshield" },
                cx,
            ))
            .child(fields)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap_2()
                    .pt_2()
                    .child(
                        app_button("private-cancel", "Cancel")
                            .outline()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(form) = &this.draft_form {
                                    if let Some(id) = &form.id {
                                        send(&json!({"action":"cancel","draft_id":id}));
                                    } else {
                                        this.cancel_draft_on_arrival =
                                            Some(form.request_id.clone());
                                    }
                                }
                                this.hide_draft(cx);
                            })),
                    )
                    .child(
                        desktop_action_button("private-submit", "Submit in desktop app")
                            .primary()
                            .flex_1()
                            .disabled(!ready)
                            .on_click(cx.listener(|this, _, _, cx| this.submit_draft(cx))),
                    ),
            )
    }

    pub(super) fn render_private_draft_handoff(
        draft: &DraftSnapshot,
        cx: &Context<'_, Self>,
    ) -> Div {
        use ui::private_submission::OperationControl;

        let progress = &draft.private_progress;
        let attention = draft.status == "attention";
        let working = draft.status == "in_progress";
        let terminal = matches!(draft.status.as_str(), "done" | "failed");
        let accent = if progress["result"] == "inclusion_unknown" {
            cx.theme().warning
        } else if draft.status == "failed" {
            cx.theme().danger
        } else if draft.warning {
            cx.theme().warning
        } else if draft.status == "done" {
            cx.theme().success
        } else {
            cx.theme().primary
        };
        let indicator = if working {
            Spinner::new()
                .icon(IconName::LoaderCircle)
                .with_size(cx.theme().font_size * 2.5)
                .color(accent)
                .into_any_element()
        } else {
            div()
                .size(rems(3.5))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_1()
                .border_color(accent)
                .when(draft.status == "done", |this| this.bg(accent))
                .child(
                    Icon::empty()
                        .path(match draft.status.as_str() {
                            "done" => "icons/check.svg",
                            "failed" => "icons/close.svg",
                            _ => "ui/icons/monitor.svg",
                        })
                        .with_size(cx.theme().font_size * 1.75)
                        .text_color(if draft.status == "done" {
                            cx.theme().background
                        } else {
                            accent
                        }),
                )
                .into_any_element()
        };
        let mut content = div()
            .w_full()
            .max_w(rems(28.0))
            .mx_auto()
            .min_w_0()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .when(!host_is_side_panel(), gpui::Styled::my_auto)
            .py_6()
            .gap_3()
            .child(indicator)
            .child(
                div()
                    .w_full()
                    .max_w_80()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_center()
                    .child(
                        app_strong_text(draft.step.clone())
                            .text_lg()
                            .whitespace_normal(),
                    )
                    .child(if draft.warning && attention {
                        Alert::warning("private-handoff-attention", draft.message.clone())
                            .small()
                            .into_any_element()
                    } else {
                        app_muted_text(draft.message.clone())
                            .whitespace_normal()
                            .into_any_element()
                    }),
            );
        if working {
            let context = rows(&progress["context"])
                .into_iter()
                .map(|mut row| {
                    if row.label == "Recipient" && row.value.chars().count() > 28 {
                        row.value = short_address(&row.value);
                    }
                    row
                })
                .collect::<Vec<_>>();
            if !context.is_empty() {
                content = content.child(
                    ui::private_submission::transaction_context(context, None)
                        .flex_none()
                        .mt_2()
                        .p_3()
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().muted),
                );
            }
        } else {
            let summary = text(progress, "summary");
            if !summary.is_empty() {
                let recipient = text(&draft.input, "recipient");
                let summary = if recipient.is_empty() {
                    summary
                } else {
                    format!("{summary} to {}", short_address(&recipient))
                };
                content = content.child(app_muted_text(summary).text_center().whitespace_normal());
            }
        }
        if let Some(hash) = progress["transaction_hash"].as_str() {
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(note(short_address(hash)).font_family(MONO))
                    .child(ui::clipboard::clipboard_with_toast(
                        SharedString::from("private-tx-hash"),
                        hash.to_owned(),
                    )),
            );
        }
        let controls = [
            (
                "stop",
                if progress["stop_retries"] == true {
                    OperationControl::StopRetries
                } else {
                    OperationControl::Stop
                },
            ),
            ("stop_waiting", OperationControl::StopWaiting),
            ("ban", OperationControl::Ban),
            ("favorite", OperationControl::Favorite),
        ]
        .into_iter()
        .filter_map(|(flag, control)| (progress[flag] == true).then_some(control))
        .collect::<Vec<_>>();
        let id = draft.id.clone();
        let execution = text(progress, "execution_id");
        let operation_actions = (!controls.is_empty()).then(|| ui::private_submission::operation_controls("private-operation", controls, move |control, _, _| {
            let control = match control { OperationControl::Stop | OperationControl::StopRetries => "stop", OperationControl::StopWaiting => "stop_waiting", OperationControl::Ban => "ban", OperationControl::Favorite => "favorite" };
            send(&json!({"action":"private_control","draft_id":id,"execution_id":execution,"control":control}));
        }).min_w_0().justify_center());
        let mut actions = div()
            .w_full()
            .max_w(rems(28.0))
            .mx_auto()
            .flex_none()
            .flex()
            .flex_col()
            .gap_2();
        if attention
            && draft.can_cancel
            && progress["stop"] != true
            && progress["stop_waiting"] != true
        {
            let id = draft.id.clone();
            actions = actions.child(
                app_button("private-cancel-approval", "Cancel")
                    .ghost()
                    .small()
                    .on_click(move |_, _, _| send(&json!({"action":"cancel","draft_id":id}))),
            );
        }
        let mut primary_actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_center()
            .gap_2();
        if terminal {
            primary_actions = primary_actions.children(operation_actions);
            if draft.can_retry {
                primary_actions = primary_actions.child(
                    app_button("private-retry", "Edit and retry")
                        .outline()
                        .flex_1()
                        .on_click(cx.listener(|this, _, window, cx| this.retry_draft(window, cx))),
                );
            }
            primary_actions = primary_actions.child(
                app_button("private-done", "Done")
                    .primary()
                    .flex_1()
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss_draft(cx))),
            );
        } else {
            primary_actions = primary_actions.child(
                app_button("private-hide", "Hide")
                    .outline()
                    .when(attention, gpui::Styled::flex_1)
                    .on_click(cx.listener(|this, _, _, cx| this.hide_draft(cx))),
            );
            if attention {
                actions = actions.children(operation_actions);
                primary_actions = primary_actions.child(
                    summon_desktop_button()
                        .when(draft.warning, |button| button.warning().outline()),
                );
            } else {
                primary_actions = primary_actions.children(operation_actions);
            }
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(
                div()
                    .id("private-progress-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(content),
            )
            .child(actions.child(primary_actions))
    }

    fn render_private_picker(picker: &PrivatePicker, cx: &Context<'_, Self>) -> Div {
        let entries = project_broadcaster_picker_entries(
            &picker.rows,
            BroadcasterPickerViewMode::Grouped,
            !picker.query.read(cx).value().trim().is_empty(),
            &picker.expanded,
            &picker.collapsed,
        );
        let mut list = div()
            .id("private-broadcaster-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&picker.scroll)
            .flex()
            .flex_col()
            .gap_1();
        for entry in entries {
            match entry {
                BroadcasterPickerEntry::Broadcaster(row) => {
                    let id = row.railgun_address.clone();
                    list = list.child(
                        app_button_base(SharedString::from(format!("private-broadcaster-{id}")))
                            .ghost()
                            .compact()
                            .w_full()
                            .h_auto()
                            .min_h_8()
                            .flex_none()
                            .px_2()
                            .py_1()
                            .accessibility_label(row.label.clone())
                            .selected(row.selected)
                            .child(ui::broadcaster_picker::render_broadcaster_picker_row(
                                &row,
                                BroadcasterPickerLayout::Compact,
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_private_picker(window, cx);
                                if let Some(form) = &mut this.draft_form {
                                    form.input["broadcaster"] = json!({"mode":"specific","id":id});
                                }
                                this.draft_changed(cx);
                            })),
                    );
                }
                BroadcasterPickerEntry::Group(group) => {
                    let command = group.clone();
                    let root = cx.entity();
                    list = list.child(ui::broadcaster_picker::render_broadcaster_picker_group(
                        group,
                        BroadcasterPickerLayout::Compact,
                        move |_, cx| {
                            root.update(cx, |root, cx| {
                                if let Some(picker) = root
                                    .draft_form
                                    .as_mut()
                                    .and_then(|form| form.private.as_mut())
                                    .and_then(|private| private.picker.as_mut())
                                {
                                    update_broadcaster_picker_group_expansion(
                                        &mut picker.expanded,
                                        &mut picker.collapsed,
                                        command.key,
                                        command.expanded,
                                        command.selected_child_address.clone(),
                                        command.revision.clone(),
                                    );
                                }
                                cx.notify();
                            });
                        },
                        false,
                    ));
                }
            }
        }
        if picker.rows.is_empty() {
            list = list.child(note(
                "No matching broadcasters. The desktop will update this list as candidates arrive.",
            ));
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(Self::back_title("Broadcasters", cx))
            .child(app_input(&picker.query).aria_label("Search broadcasters"))
            .child(ui::broadcaster_picker::render_broadcaster_picker_header(
                BroadcasterPickerLayout::Compact,
                &picker.query,
                picker.rows.len(),
                picker.total,
                picker.popover_open,
                {
                    let root = cx.entity();
                    move |open, cx| {
                        root.update(cx, |root, cx| {
                            if let Some(picker) = root
                                .draft_form
                                .as_mut()
                                .and_then(|form| form.private.as_mut())
                                .and_then(|private| private.picker.as_mut())
                            {
                                picker.popover_open = open;
                            }
                            cx.notify();
                        });
                    }
                },
            ))
            .child(list)
    }
}
