//! Editable inputs and peer-scoped desktop progress. This view never signs.
mod defaults;
mod draft_editing;
mod draft_render;
mod draft_sync;
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
