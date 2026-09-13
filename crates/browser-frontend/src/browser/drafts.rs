//! Editable inputs and peer-scoped desktop progress. This view never signs.
mod private;
use super::*;
use gpui::{Focusable as _, Task};
use gpui_component::button::ButtonVariants;
use gpui_component::{
    ActiveTheme as _, input::InputEvent, select::SelectEvent, spinner::Spinner, switch::Switch,
};
use serde_json::{Value, json};
use std::time::Duration;
use ui::controls::{FullWidthSelectItems, amount_max_button, public_action_mode_group};
use ui::fees::estimated_fees;

use public_view::AssetBalance;
use ui::gas_fee::{GasFeeEditTarget, GasFeeEditor, GasFeeEditorEvent, GasFeeMode};
use ui::recipient_picker::{
    RecipientPicker, RecipientPickerEvent, RecipientSuggestion, suggestion_index_after_move,
};

#[derive(Clone)]
struct DraftRecipient {
    id: String,
    label: String,
    address: String,
}

impl SelectItem for AssetBalance {
    type Value = String;

    fn title(&self) -> SharedString {
        self.symbol.clone().into()
    }

    fn value(&self) -> &String {
        &self.id
    }

    fn display_title(&self) -> Option<AnyElement> {
        Some(asset_choice(self).into_any_element())
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        asset_choice(self).child(
            note(format!("{} available", self.amount))
                .flex_1()
                .min_w_0()
                .truncate(),
        )
    }
}

fn asset_choice(asset: &AssetBalance) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .w_full()
        .min_w_0()
        .when(!asset.icon.is_empty(), |this| {
            this.child(img(asset.icon.clone()).size_5().flex_none())
        })
        .child(app_button_label(asset.symbol.clone()))
}

#[derive(Clone)]
pub(super) struct DraftSnapshot {
    id: String,
    request_id: String,
    revision: u64,
    input: Value,
    status: String,
    estimate: Value,
    gas_quote: Value,
    private_options: Value,
    private_progress: Value,
    recipients: Vec<DraftRecipient>,
    step: String,
    message: String,
    warning: bool,
    can_cancel: bool,
    can_retry: bool,
}
impl DraftSnapshot {
    pub(super) fn continues(&self, next: &Self) -> bool {
        self.id == next.id && self.executing() && next.executing()
    }
    pub(super) fn from_snapshot(snapshot: &JsValue) -> Option<Self> {
        if flag_field(snapshot, "locked") {
            return None;
        }
        let drafts = field(&field(snapshot, "public_view"), "drafts");
        if !js_sys::Array::is_array(&drafts) {
            return None;
        }
        let drafts = js_sys::Array::from(&drafts);
        let value = drafts.get(0);
        let json_value = |key| {
            js_sys::JSON::stringify(&field(&value, key))
                .ok()
                .and_then(|value| value.as_string())
                .and_then(|value| serde_json::from_str(&value).ok())
                .unwrap_or(Value::Null)
        };
        let id = text_field(&value, "draft_id");
        if id.is_empty() {
            return None;
        }
        Some(Self {
            id,
            request_id: text_field(&value, "request_id"),
            revision: chain_id_field(&value, "revision")?,
            input: json_value("input"),
            status: text_field(&value, "status"),
            estimate: json_value("estimate"),
            gas_quote: json_value("gas_quote"),
            private_options: json_value("private_options"),
            private_progress: json_value("private_progress"),
            recipients: js_sys::Array::from(&field(&value, "recipients"))
                .iter()
                .map(|entry| DraftRecipient {
                    id: text_field(&entry, "id"),
                    label: text_field(&entry, "label"),
                    address: text_field(&entry, "address"),
                })
                .collect(),
            step: text_field(&value, "step_label"),
            message: text_field(&value, "message"),
            warning: flag_field(&value, "warning"),
            can_cancel: flag_field(&value, "can_cancel"),
            can_retry: flag_field(&value, "can_retry"),
        })
    }
    fn executing(&self) -> bool {
        matches!(
            self.status.as_str(),
            "attention" | "in_progress" | "done" | "failed"
        )
    }
}

pub(super) struct DraftForm {
    private: Option<private::PrivateDraftForm>,
    request_id: String,
    id: Option<String>,
    revision: u64,
    sent: Option<Value>,
    sent_revision: Option<u64>,
    input: Value,
    assets: Vec<AssetBalance>,
    asset: Entity<SelectState<FullWidthSelectItems<AssetBalance>>>,
    amount: Entity<InputState>,
    setting_max: Option<String>,
    setting_recipient: Option<String>,
    recipient: Entity<InputState>,
    recipient_open: bool,
    recipient_index: Option<usize>,
    recipient_scroll: gpui::ScrollHandle,
    max_fee: Entity<InputState>,
    priority_fee: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
    pending: Option<Task<()>>,
}
impl DraftForm {
    fn sync_token_max(&mut self, window: &mut Window, cx: &mut App) {
        if self.input["wallet"].is_string() || self.input["max"] != true {
            return;
        }
        let Some(amount) = self
            .assets
            .iter()
            .find(|asset| Some(asset.id.as_str()) == self.input["asset"].as_str())
            .and_then(|asset| asset.max_amount.as_ref())
        else {
            return;
        };
        if self.amount.read(cx).value().as_ref() != amount {
            self.setting_max = Some(amount.clone());
            self.amount.update(cx, |input, cx| {
                input.set_value(amount.clone(), window, cx);
            });
        }
    }

    fn input_error(&self) -> Option<&'static str> {
        if text(&self.input, "amount").len() > 100 {
            Some("Amount is too long.")
        } else if text(&self.input, "recipient").len() > 1024 {
            Some("Recipient is too long.")
        } else if text(draft_fee(&self.input), "max_fee_gwei").len() > 100
            || text(draft_fee(&self.input), "priority_fee_gwei").len() > 100
        {
            Some("Gas fee is too long.")
        } else if text(&self.input["delivery"]["funding"]["incentive"], "percent").len() > 100 {
            Some("Incentive is too long.")
        } else {
            None
        }
    }
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

fn draft_fee(input: &Value) -> &Value {
    if input["wallet"].is_string() {
        &input["delivery"]["fee"]
    } else {
        &input["fee"]
    }
}

fn draft_fee_mut(input: &mut Value) -> &mut Value {
    if input["wallet"].is_string() {
        &mut input["delivery"]["fee"]
    } else {
        &mut input["fee"]
    }
}
fn send(command: &Value) {
    host_command(
        "public_view",
        &json!({"type": "draft", "command": command}).to_string(),
    );
}

impl GatewayView {
    pub(super) fn clear_draft_ui(&mut self) {
        self.draft = None;
        self.draft_form = None;
        self.handoff_open = false;
        self.draft_completion = None;
        self.completed_draft = None;
        self.cancel_draft_on_arrival = None;
    }

    pub(super) fn sync_draft(
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
                    || form.input["chain_id"].as_u64() != self.private_view.selected_chain
            } else {
                Some(text(&form.input, "account")) != self.public_view.selected_account
                    || form.input["chain_id"].as_u64() != self.public_view.selected_chain
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

    fn sync_draft_assets(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
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

    pub(super) fn open_draft(
        &mut self,
        kind: &str,
        asset: Option<String>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.public_view.drafts_supported {
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
                && draft.input["chain_id"].as_u64() == Some(chain)
        });
        let mut input = existing.map_or_else(
            || {
                json!({"account":account, "chain_id":chain, "kind":kind,
            "asset":"native", "amount":"", "recipient":"", "address_book_entry":null,
            "fee":{"mode":"normal"}, "mimic_railway":true, "max":false})
            },
            |draft| draft.input.clone(),
        );
        input["kind"] = kind.into();
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

    fn install_draft_form(
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

    fn draft_gas_quote(&self) -> Option<(String, String)> {
        let form = self.draft_form.as_ref()?;
        let draft = self.draft.as_ref().filter(|draft| {
            form.id.as_ref() == Some(&draft.id)
                && form.input["wallet"] == draft.input["wallet"]
                && form.input["delivery"]["mode"] == draft.input["delivery"]["mode"]
                && ["account", "chain_id", "kind", "mimic_railway"]
                    .iter()
                    .all(|key| form.input[key] == draft.input[key])
        })?;
        Some((
            draft.gas_quote["max_fee_gwei"].as_str()?.to_owned(),
            draft.gas_quote["priority_fee_gwei"].as_str()?.to_owned(),
        ))
    }

    fn handle_draft_gas_fee(
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

    fn draft_recipient_options(&self) -> Vec<RecipientSuggestion> {
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

    fn draft_recipient_query(&self, cx: &App) -> String {
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

    fn handle_draft_recipient(
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

    fn draft_changed(&mut self, cx: &mut Context<'_, Self>) {
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
    fn flush_draft(&mut self, cx: &mut Context<'_, Self>) {
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
    fn submit_draft(&mut self, cx: &mut Context<'_, Self>) {
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
    fn dismiss_draft(&mut self, cx: &mut Context<'_, Self>) {
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
    pub(super) fn hide_draft(&mut self, cx: &mut Context<'_, Self>) {
        let _ = self.retire_private_picker();
        self.handoff_open = false;
        self.draft_form = None;
        cx.notify();
    }
    fn retry_draft(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
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

    pub(super) fn render_draft_banner(&self, cx: &Context<'_, Self>) -> Option<Div> {
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

    pub(super) fn render_draft_form(&self, cx: &Context<'_, Self>) -> Div {
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
                                form.input["chain_id"].as_u64().unwrap_or_default(),
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
                        form.input["kind"] = if *shield { "shield" } else { "send" }.into();
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
                    note("Use Railway's transaction profile and fee policy.").whitespace_normal(),
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

    pub(super) fn render_draft_handoff(&self, cx: &Context<'_, Self>) -> Div {
        let Some(draft) = &self.draft else {
            return div().child(note("Waiting for the desktop app…"));
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
