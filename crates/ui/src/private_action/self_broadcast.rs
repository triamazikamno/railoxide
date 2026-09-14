//! Self-broadcast presentation. Wallet owners supply choices, formatted costs and commands.
use super::{
    APP_MONO_FONT_FAMILY, APP_TEXT_LINE_HEIGHT, App, ButtonGroup, ButtonVariants, Disableable, Div,
    ElementId, IntoElement, ParentElement, SharedString, Sizable, Styled, Window, app_muted_text,
    app_segment_button, app_strong_text, div, relative, rems, rgb, theme,
};
use gpui::{InteractiveElement as _, StatefulInteractiveElement as _, prelude::FluentBuilder as _};
use gpui_component::{Icon, IconName, alert::Alert, collapsible::Collapsible};
use serde::{Deserialize, Serialize};

pub const PUBLIC_FUNDING_DISCLOSURE: &str = "Self-broadcast links the selected gas payer, RPC metadata, and transaction timing to this action.";
pub const SPONSORSHIP_LABEL: &str = "Block builder sponsorship";
pub const SPONSORSHIP_HELP: &str = "Allows self-broadcast from an empty or underfunded Public account. A participating block builder funds the account for gas and is atomically reimbursed, with the selected incentive, by a private WETH unshield.";
pub const SPONSORSHIP_DISCLOSURE: &str = "The relay can see the submitted bundle. The builder payment is public, and the transaction links the selected Public signer to this action. Unused builder funding remains on the signer as public ETH.";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeliveryChoice {
    Broadcaster,
    SelfBroadcast,
    ExternalWallet,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FundingChoice {
    PublicBalance,
    Sponsorship,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IncentiveChoice {
    Economy,
    Standard,
    Priority,
    Custom,
}

#[derive(Clone, Copy)]
pub enum SettingsEvent {
    Funding(FundingChoice),
    Incentive(IncentiveChoice),
    RandomSigner,
}

pub struct Settings {
    pub funding: FundingChoice,
    pub incentive: IncentiveChoice,
    pub show_sponsorship: bool,
    pub sponsorship_unavailable: Option<String>,
    pub no_signers: bool,
    pub signer_error: Option<String>,
    pub incentive_error: Option<String>,
    pub random_enabled: bool,
    pub disabled: bool,
}

#[must_use]
pub fn delivery_selector(
    id: impl Into<ElementId>,
    selected: DeliveryChoice,
    self_broadcast_available: bool,
    show_external: bool,
    public_privacy_help: bool,
    disabled: bool,
    on_change: impl Fn(DeliveryChoice, &mut Window, &mut App) + 'static,
) -> Div {
    let choices = [
        (
            DeliveryChoice::Broadcaster,
            "broadcaster",
            "Public broadcaster",
        ),
        (DeliveryChoice::SelfBroadcast, "self", "Self-broadcast"),
        (
            DeliveryChoice::ExternalWallet,
            "external",
            "External wallet",
        ),
    ];
    div().child(
        ButtonGroup::new(id)
            .outline()
            .compact()
            .w_full()
            .children(
                choices
                    .into_iter()
                    .filter(|(choice, _, _)| {
                        show_external || *choice != DeliveryChoice::ExternalWallet
                    })
                    .map(|(choice, id, label)| {
                        app_segment_button(
                            id,
                            label,
                            choice == selected,
                            disabled
                                || (choice == DeliveryChoice::SelfBroadcast
                                    && !self_broadcast_available),
                            (choice == DeliveryChoice::SelfBroadcast && public_privacy_help).then(
                                || {
                                    help("self-privacy", PUBLIC_FUNDING_DISCLOSURE)
                                        .into_any_element()
                                },
                            ),
                        )
                        .flex_1()
                        .min_w_0()
                    }),
            )
            .on_click(move |selected, window, cx| {
                if let Some(index) = selected.first() {
                    on_change(choices[*index].0, window, cx);
                }
            }),
    )
}

fn help(id: &'static str, message: &'static str) -> impl IntoElement {
    let button = crate::controls::app_button_base("info")
        .ghost()
        .small()
        .compact()
        .icon(Icon::new(IconName::Info))
        .accessibility_label(message);
    div().id(id).child(button).tooltip(move |window, cx| {
        let width =
            (window.viewport_size().width - window.rem_size() * 3.0).min(window.rem_size() * 20.0);
        gpui_component::tooltip::Tooltip::element(move |_, _| {
            div()
                .max_w(width)
                .whitespace_normal()
                .line_height(relative(APP_TEXT_LINE_HEIGHT))
                .child(message)
        })
        .build(window, cx)
    })
}

fn settings_row(label: &'static str, control: impl IntoElement) -> Div {
    div()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_3()
        .child(app_muted_text(label))
        .child(div().min_w_0().max_w_full().child(control))
}

#[must_use]
pub fn signer_select<D>(
    state: &gpui::Entity<gpui_component::select::SelectState<D>>,
    sponsored: bool,
    missing: bool,
    error: bool,
    disabled: bool,
) -> Div
where
    D: gpui_component::select::SelectDelegate + 'static,
    <D::Item as gpui_component::select::SelectItem>::Value: PartialEq + Clone,
{
    div().w_full().min_w_0().child(
        gpui_component::select::Select::new(state)
            .small()
            .w_full()
            .placeholder(if missing {
                if sponsored {
                    "Transaction signer required"
                } else {
                    "Gas payer required"
                }
            } else {
                "Please select"
            })
            .menu_width(rems(27.0))
            .when(missing || error, |this| {
                this.border_color(rgb(theme::DANGER))
            })
            .disabled(disabled),
    )
}

#[must_use]
pub fn settings(
    id: impl Into<ElementId>,
    settings: Settings,
    signer: impl IntoElement,
    custom_incentive: impl IntoElement,
    gas_editor: impl IntoElement,
    on_change: impl Fn(SettingsEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let on_change = std::rc::Rc::new(on_change);
    let funding = on_change.clone();
    let incentive = on_change.clone();
    let sponsored = settings.funding == FundingChoice::Sponsorship;
    let random_label = if sponsored {
        "Choose random transaction signer"
    } else {
        "Choose random gas payer"
    };
    let disabled = settings.disabled;
    div().child(
        div()
            .id(id)
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_2p5()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .when(settings.show_sponsorship, |this| {
                this.child(settings_row(
                    "Gas funding",
                    ButtonGroup::new("funding")
                        .outline()
                        .compact()
                        .flex_wrap()
                        .children(vec![
                            app_segment_button(
                                "public",
                                "Public balance",
                                !sponsored,
                                disabled,
                                Some(
                                    help("public-privacy", PUBLIC_FUNDING_DISCLOSURE)
                                        .into_any_element(),
                                ),
                            ),
                            app_segment_button(
                                "sponsored",
                                SPONSORSHIP_LABEL,
                                sponsored,
                                disabled || settings.sponsorship_unavailable.is_some(),
                                Some(help("sponsorship-help", SPONSORSHIP_HELP).into_any_element()),
                            ),
                        ])
                        .on_click(move |selected, window, cx| {
                            if let Some(index) = selected.first() {
                                funding(
                                    SettingsEvent::Funding(if *index == 0 {
                                        FundingChoice::PublicBalance
                                    } else {
                                        FundingChoice::Sponsorship
                                    }),
                                    window,
                                    cx,
                                );
                            }
                        }),
                ))
            })
            .when_some(settings.sponsorship_unavailable, |this, reason| {
                this.child(app_muted_text(reason).whitespace_normal())
            })
            .when(sponsored, |this| {
                this.child(settings_row(
                    "Builder incentive",
                    ButtonGroup::new("incentive")
                        .outline()
                        .compact()
                        .flex_wrap()
                        .children(
                            [
                                (IncentiveChoice::Economy, "economy", "Economy 1%"),
                                (IncentiveChoice::Standard, "standard", "Standard 5%"),
                                (IncentiveChoice::Priority, "priority", "Priority 15%"),
                                (IncentiveChoice::Custom, "custom", "Custom"),
                            ]
                            .into_iter()
                            .map(|(choice, id, label)| {
                                app_segment_button(
                                    id,
                                    label,
                                    choice == settings.incentive,
                                    disabled,
                                    None,
                                )
                            }),
                        )
                        .on_click(move |selected, window, cx| {
                            if let Some(index) = selected.first() {
                                incentive(
                                    SettingsEvent::Incentive(
                                        [
                                            IncentiveChoice::Economy,
                                            IncentiveChoice::Standard,
                                            IncentiveChoice::Priority,
                                            IncentiveChoice::Custom,
                                        ][*index],
                                    ),
                                    window,
                                    cx,
                                );
                            }
                        }),
                ))
                .when(settings.incentive == IncentiveChoice::Custom, |this| {
                    this.child(settings_row("Custom incentive (1-100%)", custom_incentive))
                })
                .when_some(settings.incentive_error, |this, error| {
                    this.child(app_muted_text(error).whitespace_normal())
                })
            })
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(app_muted_text(if sponsored {
                        "Transaction signer"
                    } else {
                        "Gas payer"
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .max_w_80()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                crate::controls::app_button_base("random-signer")
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(Icon::empty().path("railgun/icons/dices.svg"))
                                    .accessibility_label(random_label)
                                    .tooltip(random_label)
                                    .disabled(disabled || !settings.random_enabled)
                                    .on_click(move |_, window, cx| {
                                        on_change(SettingsEvent::RandomSigner, window, cx);
                                    }),
                            )
                            .child(div().min_w_0().flex_1().child(signer)),
                    ),
            )
            .when(settings.no_signers, |this| {
                this.child(
                    app_muted_text(if sponsored {
                        "No active Public accounts are available as transaction signers."
                    } else {
                        "No active Public accounts are available for self-broadcast gas payment."
                    })
                    .whitespace_normal(),
                )
            })
            .when_some(settings.signer_error, |this, error| {
                this.child(Alert::warning("signer-error", error).small())
            })
            .child(gas_editor)
            .when(sponsored, |this| {
                this.child(Alert::warning("sponsorship-privacy", SPONSORSHIP_DISCLOSURE).small())
            }),
    )
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FeeDisplay {
    PublicBalance {
        expected_gas_cost: String,
        maximum_gas_cost: Option<String>,
        protocol_fee: Option<String>,
    },
    PublicBalanceError,
    Ready {
        expected_sponsorship_cost: String,
        gas_cost: String,
        builder_premium: String,
        primary_unshield_protocol_fee: Option<String>,
        expected_excess_deposit: String,
        maximum_spend: String,
        show_excess_deposit_breakdown: bool,
    },
    Error(String),
}

#[must_use]
pub fn estimated_fees(
    id: impl Into<ElementId>,
    display: &FeeDisplay,
    protocol_fee_label: String,
    open: bool,
    on_toggle: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> Div {
    if let FeeDisplay::PublicBalance {
        expected_gas_cost,
        maximum_gas_cost,
        protocol_fee,
    } = display
    {
        return crate::fees::estimated_fees(
            None,
            expected_gas_cost.clone(),
            maximum_gas_cost.clone(),
            protocol_fee.clone().map(|fee| (protocol_fee_label, fee)),
            APP_MONO_FONT_FAMILY,
        );
    }
    let card = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .p_2p5()
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(app_strong_text("Estimated fees"));
    match display {
        FeeDisplay::Ready { expected_sponsorship_cost, gas_cost, builder_premium,
            primary_unshield_protocol_fee, expected_excess_deposit, maximum_spend,
            show_excess_deposit_breakdown,
        } => card.child(
            Collapsible::new().open(open).w_full().min_w_0()
                .child(crate::controls::app_button_base(id)
                    .ghost().w_full().min_w_0().h_auto().min_h_8().px_0().py_1()
                    .accessibility_label("Expected transaction cost")
                    .child(div().w_full().min_w_0().flex().items_center().gap_3()
                        .child(div().min_w_0().whitespace_normal().child("Expected transaction cost"))
                        .child(app_strong_text(expected_sponsorship_cost.clone())
                            .flex_1().min_w_0().whitespace_normal().text_right()
                            .font_family(APP_MONO_FONT_FAMILY))
                        .child(Icon::new(if open { IconName::ChevronUp } else { IconName::ChevronDown })
                            .xsmall().flex_none().text_color(rgb(theme::TEXT_MUTED))))
                    .on_click(move |_, window, cx| on_toggle(!open, window, cx)))
                .content(div().min_w_0().flex().flex_col().gap_1().px_2().py_2()
                    .border_t_1().border_color(rgb(theme::BORDER))
                    .child(cost_row("Expected network gas", gas_cost.clone()))
                    .child(cost_row("Builder premium", builder_premium.clone()))
                    .when(*show_excess_deposit_breakdown, |this| this
                        .child(div().my_1().border_t_1().border_color(rgb(theme::BORDER)))
                        .child(cost_row("Unshielded up front", maximum_spend.clone()))
                        .child(cost_row("Excess deposited to signer", expected_excess_deposit.clone()))
                        .child(app_muted_text("Unused builder funding remains on the signer as public ETH.")
                            .min_w_0().whitespace_normal()))))
            .when_some(primary_unshield_protocol_fee.clone(), |this, fee| this
                .child(cost_row(protocol_fee_label, fee))),
        FeeDisplay::PublicBalanceError => card.child(Alert::error(id,
            "Fee estimate is unavailable for the current inputs.").small()),
        FeeDisplay::Error(message) => card.child(Alert::error(id, message.clone()).small()),
        FeeDisplay::PublicBalance { .. } => card,
    }
}

fn cost_row(label: impl Into<SharedString>, value: String) -> Div {
    div()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_start()
        .justify_between()
        .gap_2()
        .text_size(rems(0.875))
        .line_height(relative(APP_TEXT_LINE_HEIGHT))
        .child(app_muted_text(label).min_w_0().whitespace_normal())
        .child(
            app_strong_text(value)
                .min_w_0()
                .whitespace_normal()
                .text_right()
                .font_family(APP_MONO_FONT_FAMILY),
        )
}

#[must_use]
pub fn signer_trigger_row(label: String, address_label: String) -> Div {
    div()
        .min_w_0()
        .flex()
        .items_center()
        .gap_1()
        .child(div().min_w_0().truncate().child(label))
        .child(app_muted_text(address_label).text_size(rems(0.875)))
}

#[must_use]
pub fn signer_menu_row(label: String, address_label: String, balance_label: String) -> Div {
    div()
        .w_full()
        .py_1()
        .min_w_0()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .gap_1()
                .child(app_strong_text(label.clone()).min_w_0().truncate())
                .child(app_muted_text(address_label)),
        )
        .child(
            app_muted_text(balance_label)
                .debug_selector(move || format!("gas-payer-balance-{label}"))
                .flex_none()
                .text_right(),
        )
}
