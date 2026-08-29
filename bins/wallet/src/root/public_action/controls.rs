use super::*;

const MIMIC_RAILWAY_TITLE: &str = "Mimic Railway";
const MIMIC_RAILWAY_INTRO: &str = "Construct this shield like Railway so on-chain observers cannot easily identify which wallet created it.";
const MIMIC_RAILWAY_TRADEOFF_ALLOWANCE: &str =
    "ERC-20 shields may grant the RAILGUN contract an unlimited token allowance.";
const MIMIC_RAILWAY_TRADEOFF_GAS: &str = "Gas limits and maximum fee estimates may be higher.";
const MIMIC_RAILWAY_PUBLIC_VALUES: &str = "Public address, token, or amount";
const MIMIC_RAILWAY_TIMING: &str = "Transaction timing or later unshield behavior";

fn render_mimic_railway_tooltip_body() -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(MIMIC_RAILWAY_INTRO),
        )
        .child(render_mimic_railway_tooltip_section(
            "Tradeoffs",
            [MIMIC_RAILWAY_TRADEOFF_ALLOWANCE, MIMIC_RAILWAY_TRADEOFF_GAS],
        ))
        .child(render_mimic_railway_tooltip_section(
            "Does not hide",
            [MIMIC_RAILWAY_PUBLIC_VALUES, MIMIC_RAILWAY_TIMING],
        ))
        .into_any_element()
}

fn render_mimic_railway_tooltip_section(
    heading: &'static str,
    bullets: [&'static str; 2],
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(theme::TEXT))
                .child(heading),
        )
        .children(bullets.map(render_mimic_railway_tooltip_bullet))
}

fn render_mimic_railway_tooltip_bullet(label: &'static str) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap_1()
        .child(div().text_color(rgb(theme::TEXT_MUTED)).child("-"))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(label),
        )
}

pub(in crate::root) fn render_mimic_railway_shield_control(
    root: Entity<WalletRoot>,
    checked: bool,
    disabled: bool,
) -> gpui::Div {
    let toggle_root = root;
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(
            Checkbox::new("wallet-public-shield-mimic-railway")
                .label(MIMIC_RAILWAY_TITLE)
                .checked(checked)
                .small()
                .disabled(disabled)
                .on_click(move |checked, _window, cx| {
                    toggle_root.update(cx, |root, cx| {
                        root.public_form.mimic_railway_shield = *checked;
                        root.invalidate_public_action_gas_fee_quote(PublicActionMode::Shield);
                        root.refresh_public_action_gas_fee_quote(PublicActionMode::Shield, cx);
                        root.set_public_action_error(PublicActionMode::Shield, None);
                        cx.notify();
                    });
                }),
        )
        .child(render_private_action_info_icon_with_body(
            "wallet-public-shield-mimic-railway-info".into(),
            MIMIC_RAILWAY_TITLE,
            render_mimic_railway_tooltip_body,
        ))
}

pub(in crate::root) fn render_public_action_amount_input(
    root: Entity<WalletRoot>,
    mode: PublicActionMode,
    input: &Entity<InputState>,
    label: String,
    max_label: Option<String>,
    disabled: bool,
) -> gpui::Div {
    let max_root = root;
    let max_id = match mode {
        PublicActionMode::Shield => "wallet-public-shield-max",
        PublicActionMode::Send => "wallet-public-send-max",
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(app_muted_text(label))
                .children(max_label.map(|label| {
                    app_button(max_id, format!("Max: {label}"))
                        .link()
                        .xsmall()
                        .compact()
                        .disabled(disabled)
                        .on_click(move |_event, window, cx| {
                            max_root.update(cx, |root, cx| {
                                root.set_public_action_amount_to_max(mode, window, cx);
                            });
                        })
                })),
        )
        .child(app_input(input).disabled(disabled))
}

pub(in crate::root) fn public_action_segment_button(
    id: SharedString,
    label: &'static str,
    icon: impl Into<Icon>,
    selected: bool,
) -> Button {
    let button = Button::new(id)
        .flex_1()
        .min_w(px(0.0))
        .selected(selected)
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .gap_1()
                .text_size(APP_TEXT_SIZE)
                .child(icon.into().small())
                .child(label),
        );
    if selected { button.primary() } else { button }
}

pub(in crate::root) fn public_send_kind_segment_button(
    id: SharedString,
    label: &'static str,
    selected: bool,
) -> Button {
    Button::new(id)
        .selected(selected)
        .child(div().text_size(APP_TEXT_SIZE).child(label))
}

pub(in crate::root) fn public_action_title_row(
    label: String,
    icon_path: Option<WalletIconSource>,
) -> gpui::Div {
    div().flex().items_center().gap_1().child(token_label_row(
        SharedString::from(label),
        icon_path,
        px(20.0),
    ))
}

pub(in crate::root) fn public_action_context_row(label: &'static str, value: String) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(app_muted_text(label))
        .child(
            app_strong_text(value)
                .text_size(px(13.0))
                .font_family(APP_MONO_FONT_FAMILY),
        )
}

pub(in crate::root) fn render_public_action_fee_estimate(
    display: &PublicActionFeeDisplay,
    gas_pending: bool,
) -> gpui::Div {
    let expected_gas_cost = display.expected_gas_cost.clone().unwrap_or_else(|| {
        if gas_pending {
            "Estimating...".to_string()
        } else {
            "Unavailable".to_string()
        }
    });
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .p(px(10.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(app_strong_text("Estimated fees"))
        .when_some(display.gas_limit, |this, gas_limit| {
            this.child(public_action_fee_row(
                "Gas limit",
                format_gas_limit(gas_limit),
            ))
        })
        .child(public_action_fee_row(
            "Expected gas cost",
            expected_gas_cost,
        ))
        .when_some(display.visible_maximum_gas_cost(), |this, maximum| {
            this.child(public_action_muted_fee_row(
                "Maximum gas cost",
                maximum.to_string(),
            ))
        })
        .when_some(display.protocol_fee.as_ref(), |this, protocol_fee| {
            this.child(public_action_fee_row(
                public_action_protocol_fee_label(RAILGUN_PROTOCOL_FEE_BPS),
                protocol_fee.clone(),
            ))
        })
}

pub(in crate::root) fn render_public_advanced_transaction_estimate(
    chain_id: u64,
    estimate: &PublicAdvancedTransactionEstimate,
    expected_usd_micro_value: Option<U256>,
    maximum_usd_micro_value: Option<U256>,
) -> AnyElement {
    let expected_token_value =
        format_native_token_amount_for_display(chain_id, estimate.expected_gas_cost);
    let maximum_token_value =
        format_native_token_amount_for_display(chain_id, estimate.max_gas_cost);
    div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_2()
        .p(px(10.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(app_strong_text("Estimated fees"))
        .child(public_action_fee_row(
            "Gas limit",
            format_gas_limit(estimate.gas_limit),
        ))
        .child(public_action_fee_row(
            "Expected gas cost",
            format_value_with_usd_label(
                expected_token_value,
                estimate.expected_gas_cost,
                Some(18),
                expected_usd_micro_value,
                false,
            ),
        ))
        .when(
            public_action_maximum_gas_cost_is_significant(
                estimate.expected_gas_cost,
                estimate.max_gas_cost,
            ),
            |this| {
                this.child(public_action_muted_fee_row(
                    "Maximum gas cost",
                    format_value_with_usd_label(
                        maximum_token_value,
                        estimate.max_gas_cost,
                        Some(18),
                        maximum_usd_micro_value,
                        false,
                    ),
                ))
            },
        )
        .into_any_element()
}

fn public_action_fee_row(label: impl Into<SharedString>, value: String) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .child(app_muted_text(label).flex_none())
        .child(
            app_strong_text(value)
                .min_w(px(0.0))
                .text_size(px(13.0))
                .font_family(APP_MONO_FONT_FAMILY)
                .whitespace_normal(),
        )
}

fn public_action_muted_fee_row(label: &'static str, value: String) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .child(app_muted_text(label).flex_none())
        .child(
            app_muted_text(value)
                .min_w(px(0.0))
                .text_size(px(13.0))
                .font_family(APP_MONO_FONT_FAMILY)
                .whitespace_normal(),
        )
}

pub(in crate::root) fn render_public_action_active_status_notice(
    root: Entity<WalletRoot>,
    mode: PublicActionMode,
    title_override: Option<&str>,
    step: &PublicActionStepState,
    requires_device_approval: bool,
    command_available: bool,
) -> gpui::Div {
    let step_kind = step.step;
    let discard_available = public_action_discard_attempt_available(command_available, step);
    let view_root = root.clone();
    let discard_root = root;
    let title = title_override.map_or_else(
        || match mode {
            PublicActionMode::Shield => "Public shield".to_owned(),
            PublicActionMode::Send => "Public send".to_owned(),
        },
        str::to_owned,
    );
    let title = format!(
        "{title} {}",
        if step.status == PublicActionStepStatus::Error {
            "needs attention"
        } else {
            "in progress"
        }
    );
    let detail = format!(
        "{}: {}",
        public_action_step_label(step.step),
        public_action_step_detail_for_context(
            step.step,
            step.status,
            requires_device_approval,
            step.tx_hash.is_some(),
        )
    );
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .p(px(10.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::INFO))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_1()
                .child(app_strong_text(title))
                .child(app_muted_text(detail).whitespace_normal()),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .justify_end()
                .gap_2()
                .child(
                    app_button(
                        SharedString::from(format!(
                            "wallet-public-action-{}-view-progress",
                            public_action_step_id(step_kind)
                        )),
                        "View status",
                    )
                    .outline()
                    .small()
                    .on_click(move |_event, window, cx| {
                        view_root.update(cx, |root, cx| {
                            root.show_public_action_progress_dialog(window, cx);
                        });
                    }),
                )
                .when(discard_available, |this| {
                    this.child(
                        app_button(
                            SharedString::from(format!(
                                "wallet-public-action-{}-discard-attempt",
                                public_action_step_id(step_kind)
                            )),
                            "Discard attempt",
                        )
                        .danger()
                        .small()
                        .on_click(move |_event, _window, cx| {
                            discard_root.update(cx, |root, cx| {
                                root.discard_public_action_attempt(cx);
                            });
                        }),
                    )
                }),
        )
}

pub(in crate::root) fn render_public_action_progress_footer(
    root: Entity<WalletRoot>,
    action: ProgressFooterAction,
) -> gpui::Div {
    let button_root = root;
    let (id, label) = match action {
        ProgressFooterAction::Stop => ("wallet-public-action-stop", "Stop"),
        ProgressFooterAction::Close => ("wallet-public-action-close", "Close"),
    };
    let button = app_button(id, label).small().flex_none();
    let button = match action {
        ProgressFooterAction::Stop => button.danger().icon(Icon::new(RailgunActionIcon::Square)),
        ProgressFooterAction::Close => button.outline(),
    };
    div()
        .w_full()
        .flex()
        .justify_end()
        .pt(px(2.0))
        .child(button.on_click(move |_event, window, cx| {
            button_root.update(cx, |root, cx| match action {
                ProgressFooterAction::Stop => root.stop_public_action_progress(cx),
                ProgressFooterAction::Close => root.close_public_action_progress_dialog(window, cx),
            });
        }))
}

pub(in crate::root) fn public_action_closed_active_step(
    steps: &[PublicActionStepState],
) -> Option<&PublicActionStepState> {
    steps
        .iter()
        .find(|step| step.status == PublicActionStepStatus::Pending)
        .or_else(|| {
            steps
                .iter()
                .find(|step| step.status == PublicActionStepStatus::Error)
        })
}

pub(in crate::root) fn public_action_closed_status_step(
    steps: &[PublicActionStepState],
    lifecycle: PublicActionProgressLifecycle,
) -> Option<&PublicActionStepState> {
    public_action_closed_active_step(steps).or_else(|| {
        (lifecycle == PublicActionProgressLifecycle::DialogReplacedWhileConfirmedHistoryRemains)
            .then(|| steps.last())
            .flatten()
    })
}

pub(in crate::root) const fn public_action_mode_verb(mode: PublicActionMode) -> &'static str {
    match mode {
        PublicActionMode::Shield => "Shield",
        PublicActionMode::Send => "Send",
    }
}

pub(in crate::root) fn public_action_max_label(
    entry: &PublicBalanceEntry,
    native_gas_reserve: Option<U256>,
) -> Option<String> {
    if entry.asset.id == PublicAssetId::Native {
        let max_amount =
            public_action_max_amount_after_reserve(entry.amount.amount()?, native_gas_reserve?)?;
        let max_amount = PublicBalanceAmount::Available(max_amount);
        return Some(format!(
            "{} {} after est. gas",
            public_balance_amount_label(&max_amount, entry.asset.decimals),
            entry.asset.symbol,
        ));
    }
    entry.amount.amount().map(|_| {
        format!(
            "{} {}",
            public_balance_amount_label(&entry.amount, entry.asset.decimals),
            entry.asset.symbol,
        )
    })
}

pub(in crate::root) fn public_action_max_amount_after_reserve(
    amount: U256,
    reserve: U256,
) -> Option<U256> {
    (amount > reserve).then_some(amount - reserve)
}

pub(in crate::root) fn public_action_asset_label(
    chain_id: u64,
    asset: PublicAssetId,
    registry: Option<&wallet_ops::settings::EffectiveTokenRegistry>,
) -> String {
    match asset {
        PublicAssetId::Native => native_token_display_label(chain_id).to_string(),
        PublicAssetId::Erc20(_) => public_asset_label(chain_id, asset, registry),
    }
}
