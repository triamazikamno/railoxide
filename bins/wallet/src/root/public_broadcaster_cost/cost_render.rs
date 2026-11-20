#[allow(clippy::wildcard_imports)]
use super::*;

fn render_transaction_fee_breakdown(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    display: &PublicBroadcasterCostDisplay<'_>,
    anchor_cache: &TokenAnchorRateCache,
    open: bool,
    custom_fee: bool,
    automatic_maximum_fee: Option<U256>,
    edit_action: Option<AnyElement>,
) -> impl IntoElement {
    let mut total = display.fee_amount_with_usd(anchor_cache);
    if custom_fee {
        total.push_str(" · Custom");
    } else if let Some(maximum) = automatic_maximum_fee {
        let fees = display.fee_display();
        // Round up so the shown maximum is never below the enforced one.
        let value = format_token_amount_ceiling_for_display(
            fees.chain_id,
            fees.fee_token,
            maximum,
            fees.registry,
        );
        total.push_str(" · Max ");
        total.push_str(&fees.token_amount_value_with_usd(
            value,
            anchor_cache,
            fees.fee_token,
            maximum,
            false,
        ));
    }
    ui::private_action::transaction_fee_breakdown(
        delivery_element_id(key, kind, "transaction-fee-breakdown"),
        total,
        display.fee_rows(anchor_cache),
        display.gas_value(),
        open,
        edit_action,
        move |open, _, cx| {
            root.update(cx, |root, cx| {
                root.set_transaction_fee_breakdown_open(kind, key, open, cx);
            });
        },
    )
}

pub(in crate::root) fn render_public_broadcaster_tx_hash_row(
    tx_hash: String,
    button_id: SharedString,
) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(app_muted_text("Tx hash").flex_none())
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    app_strong_text(short_hash(&tx_hash)).font_family(theme::APP_MONO_FONT_FAMILY),
                )
                .child(clipboard_with_toast(button_id, tx_hash)),
        )
}

pub(in crate::root) fn render_public_broadcaster_cost_estimate(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    asset: &UnshieldAsset,
    estimate: &PublicBroadcasterCostEstimate,
    fee_anchor_rate: Option<U256>,
    registry: Option<&EffectiveTokenRegistry>,
    anchor_cache: &TokenAnchorRateCache,
    transaction_fee_breakdown_open: bool,
    refreshing: bool,
    custom_fee_amount: Option<U256>,
    fee_editable: bool,
) -> gpui::Div {
    let display =
        PublicBroadcasterCostDisplay::from_estimate(asset, estimate, fee_anchor_rate, registry);
    ui::private_action::estimated_outcome(
        broadcaster_candidate_label(display.broadcaster),
        display.outcome_rows(anchor_cache),
        render_transaction_fee_breakdown(
            root.clone(),
            key,
            kind,
            &display,
            anchor_cache,
            transaction_fee_breakdown_open,
            custom_fee_amount.is_some(),
            automatic_private_fee_bounds(estimate)
                .ok()
                .map(wallet_ops::PublicBroadcasterApprovalBounds::maximum_fee),
            fee_editable.then(|| {
                crate::root::private_action::fee_editor::fee_edit_button(root.clone(), kind, key)
                    .into_any_element()
            }),
        ),
        public_broadcaster_estimate_shape(estimate),
        delivery_element_id(key, kind, "refresh-estimate"),
        refreshing,
        move |_, cx| {
            root.update(cx, |root, cx| {
                root.reroll_public_broadcaster_cost_estimate(kind, key, cx);
            });
        },
    )
}

pub(in crate::root) fn public_broadcaster_estimate_shape(
    estimate: &PublicBroadcasterCostEstimate,
) -> String {
    format!(
        "Shape: {} proofs · {} inputs · {} private outputs · {} public outputs · {} RelayAdapt calls{}",
        estimate.transaction_count,
        estimate.input_count,
        estimate.private_output_count,
        estimate.public_output_count,
        estimate.relay_call_count,
        if estimate.uses_relay_adapt {
            " · RelayAdapt"
        } else {
            ""
        }
    )
}

pub(in crate::root) const fn public_broadcaster_cost_status(
    pending: bool,
    estimating: bool,
) -> Option<CostEstimateStatus> {
    if pending {
        None
    } else if estimating {
        Some(CostEstimateStatus::Estimating)
    } else {
        None
    }
}

pub(in crate::root) const fn public_broadcaster_cost_status_text(
    status: CostEstimateStatus,
) -> (&'static str, &'static str) {
    match status {
        CostEstimateStatus::Estimating => (
            "Estimating public broadcaster cost...",
            "Using current gas price, transaction fee rate, and selected private note shape.",
        ),
    }
}

pub(in crate::root) fn render_public_broadcaster_cost_status(
    _tick: usize,
    status: CostEstimateStatus,
) -> gpui::Div {
    let (title, detail) = public_broadcaster_cost_status_text(status);
    div()
        .flex()
        .items_center()
        .gap_3()
        .p(px(12.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(
            Spinner::new()
                .icon(IconName::LoaderCircle)
                .color(rgb(theme::INFO).into())
                .with_size(px(18.0)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    app_shimmer_text(title)
                        .text_color(rgb(theme::TEXT))
                        .font_weight(FontWeight::MEDIUM),
                )
                .child(app_muted_text(detail)),
        )
}

pub(in crate::root) fn cost_estimate_detail_text(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_color(rgb(theme::TEXT_SUBTLE))
        .text_size(COST_ESTIMATE_DETAIL_TEXT_SIZE)
        .line_height(px(15.0))
        .child(text.into())
}
