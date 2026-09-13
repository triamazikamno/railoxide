use super::*;
use ui::private_action::self_broadcast::{
    self as shared, DeliveryChoice, FundingChoice, IncentiveChoice, SettingsEvent,
};

#[derive(Clone, PartialEq, Eq)]
struct SignerChoice {
    id: String,
    label: String,
    address: String,
    balance: String,
    random_candidate: bool,
    unavailable: Option<String>,
}
impl SelectItem for SignerChoice {
    type Value = String;
    fn title(&self) -> SharedString {
        self.label.clone().into()
    }
    fn value(&self) -> &String {
        &self.id
    }
    fn display_title(&self) -> Option<AnyElement> {
        Some(
            shared::signer_trigger_row(self.label.clone(), self.address.clone()).into_any_element(),
        )
    }
    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        shared::signer_menu_row(
            self.label.clone(),
            self.address.clone(),
            self.balance.clone(),
        )
    }
}

pub(super) struct SelfBroadcastForm {
    signer: Entity<SelectState<FullWidthSelectItems<SignerChoice>>>,
    choices: Vec<SignerChoice>,
    _signer_subscription: Subscription,
    incentive: Entity<InputState>,
    _incentive_subscription: Subscription,
    saved_delivery: Value,
    saved_broadcaster: Value,
    saved_incentive: Value,
}

const BROADCASTER_FIELDS: [&str; 4] = [
    "fee_token",
    "broadcaster",
    "allow_out_of_range",
    "favorites_only",
];

fn broadcaster_fields(input: &Value) -> Value {
    BROADCASTER_FIELDS
        .into_iter()
        .map(|key| (key.to_owned(), input[key].clone()))
        .collect()
}

impl SelfBroadcastForm {
    pub(super) fn new(
        input: &Value,
        window: &mut Window,
        cx: &mut Context<'_, GatewayView>,
    ) -> Self {
        let signer = cx.new(|cx| {
            SelectState::new(
                FullWidthSelectItems::new(Vec::<SignerChoice>::new()),
                None,
                window,
                cx,
            )
            .searchable(true)
        });
        let signer_subscription = cx.subscribe(
            &signer,
            |this, _, event: &SelectEvent<FullWidthSelectItems<SignerChoice>>, cx| {
                if let SelectEvent::Confirm(Some(id)) = event
                    && let Some(form) = &mut this.draft_form
                    && form.input["delivery"]["mode"] == "self_broadcast"
                    && form.input["delivery"]["signer"] != *id
                {
                    form.input["delivery"]["signer"] = id.clone().into();
                    this.draft_changed(cx);
                }
            },
        );
        let incentive = window.with_text_style(
            Some(gpui::TextStyleRefinement {
                font_family: Some(cx.theme().font_family.clone()),
                ..Default::default()
            }),
            |window| {
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("1-100")
                        .default_value(
                            input["delivery"]["funding"]["incentive"]["percent"]
                                .as_str()
                                .unwrap_or("5"),
                        )
                })
            },
        );
        let incentive_subscription =
            cx.subscribe(&incentive, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some(form) = &mut this.draft_form
                    && form.input["delivery"]["funding"]["incentive"]["mode"] == "custom"
                {
                    let value = input.read(cx).value().to_string();
                    if form.input["delivery"]["funding"]["incentive"]["percent"] == value {
                        return;
                    }
                    form.input["delivery"]["funding"]["incentive"]["percent"] = value.into();
                    this.draft_changed(cx);
                }
            });
        Self {
            signer,
            choices: Vec::new(),
            _signer_subscription: signer_subscription,
            incentive,
            _incentive_subscription: incentive_subscription,
            saved_delivery: input["delivery"].clone(),
            saved_broadcaster: broadcaster_fields(input),
            saved_incentive: input["delivery"]["funding"]["incentive"].clone(),
        }
    }

    pub(super) fn sync(
        &mut self,
        input: &Value,
        options: &Value,
        window: &mut Window,
        cx: &mut Context<'_, GatewayView>,
    ) {
        let choices: Vec<_> = options["signers"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|choice| SignerChoice {
                id: text(choice, "id"),
                label: text(choice, "label"),
                address: text(choice, "address_label"),
                balance: text(choice, "balance_label"),
                random_candidate: choice["random_candidate"] == true,
                unavailable: choice["unavailable"].as_str().map(str::to_owned),
            })
            .collect();
        if self.choices != choices {
            self.choices.clone_from(&choices);
            self.signer.update(cx, |state, cx| {
                state.set_items(FullWidthSelectItems::new(choices), window, cx);
            });
        }
        let selected = text(&input["delivery"], "signer");
        if self.signer.read(cx).selected_value() != Some(&selected) {
            self.signer.update(cx, |state, cx| {
                state.set_selected_value(&selected, window, cx);
            });
        }
    }
}

impl GatewayView {
    pub(super) fn switch_private_delivery(
        &mut self,
        choice: DeliveryChoice,
        cx: &mut Context<'_, Self>,
    ) {
        if choice == DeliveryChoice::ExternalWallet {
            return;
        }
        let wants_self = choice == DeliveryChoice::SelfBroadcast;
        if wants_self && !self.private_view.self_broadcast_supported {
            return;
        }
        let Some(form) = &mut self.draft_form else {
            return;
        };
        if wants_self == (form.input["delivery"]["mode"] == "self_broadcast") {
            return;
        }
        let Some(private) = &mut form.private else {
            return;
        };
        private.picker = None;
        let controls = &mut private.self_broadcast;
        if wants_self {
            controls.saved_broadcaster = broadcaster_fields(&form.input);
            for key in BROADCASTER_FIELDS {
                form.input
                    .as_object_mut()
                    .expect("private draft")
                    .remove(key);
            }
            if !controls.saved_delivery.is_object() {
                let signer = private.display.as_ref().map_or(Value::Null, |display| {
                    display.options["self_broadcast"]["default_signer"].clone()
                });
                controls.saved_delivery = json!({"mode":"self_broadcast", "signer":signer,
                    "funding":{"mode":"public_balance"}, "fee":{"mode":"auto"}});
            }
            form.input["delivery"] = controls.saved_delivery.clone();
        } else {
            controls.saved_delivery = form
                .input
                .as_object_mut()
                .expect("private draft")
                .remove("delivery")
                .unwrap_or(Value::Null);
            let fallback = json!({"fee_token":form.input["asset"],"broadcaster":{"mode":"random"},"allow_out_of_range":false,"favorites_only":false});
            let restored = if controls.saved_broadcaster["broadcaster"].is_object() {
                &controls.saved_broadcaster
            } else {
                &fallback
            };
            for key in BROADCASTER_FIELDS {
                form.input[key] = restored[key].clone();
            }
        }
        self.draft_changed(cx);
    }

    fn private_self_broadcast_settings(
        &mut self,
        event: SettingsEvent,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = &mut self.draft_form else {
            return;
        };
        if form.input["delivery"]["mode"] != "self_broadcast" {
            return;
        }
        let Some(private) = &mut form.private else {
            return;
        };
        match event {
            SettingsEvent::Funding(funding) => {
                if form.input["delivery"]["funding"]["mode"] == "sponsorship" {
                    private.self_broadcast.saved_incentive =
                        form.input["delivery"]["funding"]["incentive"].clone();
                }
                form.input["delivery"]["funding"] = if funding == FundingChoice::Sponsorship {
                    let incentive = if private.self_broadcast.saved_incentive.is_object() {
                        private.self_broadcast.saved_incentive.clone()
                    } else {
                        json!({"mode":"standard"})
                    };
                    json!({"mode":"sponsorship","incentive":incentive})
                } else {
                    json!({"mode":"public_balance"})
                };
            }
            SettingsEvent::Incentive(incentive) => {
                form.input["delivery"]["funding"]["incentive"] = match incentive {
                    IncentiveChoice::Economy => json!({"mode":"economy"}),
                    IncentiveChoice::Standard => json!({"mode":"standard"}),
                    IncentiveChoice::Priority => json!({"mode":"priority"}),
                    IncentiveChoice::Custom => {
                        json!({"mode":"custom", "percent":private.self_broadcast.incentive.read(cx).value().to_string()})
                    }
                };
            }
            SettingsEvent::RandomSigner => {
                if !self.draft.as_ref().is_some_and(|draft| {
                    form.id.as_ref() == Some(&draft.id)
                        && form.revision == draft.revision
                        && form.input == draft.input
                }) {
                    return;
                }
                let choices: Vec<_> = private
                    .self_broadcast
                    .choices
                    .iter()
                    .filter(|choice| choice.random_candidate)
                    .collect();
                if choices.is_empty() {
                    return;
                }
                // Math.random is in [0, 1); the bounded native account projection fits exactly
                // in f64 on this wasm32-only frontend. This is a UI choice, not key generation.
                #[allow(clippy::cast_sign_loss, clippy::cast_precision_loss)]
                let index = (js_sys::Math::random() * choices.len() as f64) as usize;
                form.input["delivery"]["signer"] = choices[index].id.clone().into();
            }
        }
        self.draft_changed(cx);
    }

    pub(super) fn render_private_self_broadcast(
        &self,
        options: &Value,
        current: Option<&DraftSnapshot>,
        cx: &Context<'_, Self>,
    ) -> Div {
        let Some(form) = &self.draft_form else {
            return div();
        };
        let Some(private) = &form.private else {
            return div();
        };
        let controls = &private.self_broadcast;
        let options = &options["self_broadcast"];
        let sponsored = form.input["delivery"]["funding"]["mode"] == "sponsorship";
        let missing = !controls.choices.is_empty()
            && !controls
                .choices
                .iter()
                .any(|choice| choice.id == form.input["delivery"]["signer"]);
        let disabled = !self.private_view.self_broadcast_supported;
        let gas = GasFeeEditor::new(
            "private-gas",
            &form.max_fee,
            &form.priority_fee,
            cx.listener(|this, event: &GasFeeEditorEvent, window, cx| {
                this.handle_draft_gas_fee(*event, window, cx);
            }),
        )
        .mode(if draft_fee(&form.input)["mode"] == "custom" {
            GasFeeMode::Custom
        } else {
            GasFeeMode::Auto
        })
        .quote(self.draft_gas_quote())
        .disabled(disabled)
        .refreshing(current.is_none_or(|draft| draft.status == "estimating"));
        let gas = div()
            .min_w_0()
            .child(gas)
            .when(form.input_error() == Some("Gas fee is too long."), |this| {
                this.child(note("Gas fee is too long."))
            })
            .when_some(options["gas_error"].as_str(), |this, error| {
                this.child(note(error.to_owned()).whitespace_normal())
            })
            .when_some(
                current.and_then(|draft| draft.estimate["gas_error"].as_str()),
                |this, error| this.child(note(error.to_owned()).whitespace_normal()),
            );
        shared::settings(
            "private-self-settings",
            shared::Settings {
                funding: if sponsored {
                    FundingChoice::Sponsorship
                } else {
                    FundingChoice::PublicBalance
                },
                incentive: match form.input["delivery"]["funding"]["incentive"]["mode"].as_str() {
                    Some("economy") => IncentiveChoice::Economy,
                    Some("priority") => IncentiveChoice::Priority,
                    Some("custom") => IncentiveChoice::Custom,
                    _ => IncentiveChoice::Standard,
                },
                show_sponsorship: options["show_sponsorship"] == true,
                sponsorship_unavailable: options["sponsorship_unavailable"]
                    .as_str()
                    .map(str::to_owned),
                no_signers: controls.choices.is_empty(),
                signer_error: options["signer_error"].as_str().map(str::to_owned),
                incentive_error: if form.input_error() == Some("Incentive is too long.") {
                    Some("Incentive is too long.".into())
                } else {
                    options["incentive_error"].as_str().map(str::to_owned)
                },
                random_enabled: current.is_some()
                    && controls
                        .choices
                        .iter()
                        .any(|choice| choice.random_candidate),
                disabled,
            },
            shared::signer_select(
                &controls.signer,
                sponsored,
                missing,
                options["signer_error"].is_string(),
                disabled || controls.choices.is_empty(),
            ),
            ui::controls::app_input(&controls.incentive)
                .disabled(disabled)
                .w_40(),
            gas,
            {
                let root = cx.entity();
                move |event, _, cx| {
                    root.update(cx, |root, cx| {
                        root.private_self_broadcast_settings(event, cx);
                    });
                }
            },
        )
    }
}
