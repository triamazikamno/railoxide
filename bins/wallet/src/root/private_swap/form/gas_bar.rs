#[allow(clippy::wildcard_imports)]
use super::*;

/// The gas bar of a review. Positions are linear in the minimum: the bar ends at the best case
/// after the price tolerance, where solvers pay all gas, and starts where the user pays all of
/// it, or at zero when the gas estimate exceeds the swap.
#[derive(Clone, Copy)]
pub(super) struct GasBar {
    /// The best case after the price tolerance, before the shield fee.
    tolerated: U256,
    estimate: U256,
    /// The gas the bar spans: the estimate, or the tolerated best case when gas exceeds it.
    span: U256,
}

impl GasBar {
    pub(super) fn of(review: &SwapReview) -> Self {
        Self::new(
            review.best_case(),
            review.slippage_bps(),
            review.gas_estimate(),
        )
    }

    /// The bar of an order with this best case, price tolerance and gas estimate.
    pub(super) fn new(best_case: U256, slippage_bps: u32, estimate: U256) -> Self {
        let tolerance = U256::from(10_000_u32.saturating_sub(slippage_bps));
        let tolerated = best_case.saturating_mul(tolerance) / U256::from(10_000_u32);
        Self {
            tolerated,
            estimate,
            span: estimate.min(tolerated),
        }
    }

    /// The gas estimate leaves nothing of the swap, so the bar starts at zero.
    pub(super) fn gas_exceeds(&self) -> bool {
        self.estimate >= self.tolerated
    }

    /// The largest share that leaves a positive minimum.
    pub(super) fn max_share_bps(&self) -> u16 {
        if !self.gas_exceeds() || self.estimate.is_zero() {
            return GAS_SHARE_LOOSE_BPS;
        }
        // The allowance, `ceil(estimate * share / 10000)`, must stay below the tolerated best.
        (self.tolerated.saturating_sub(U256::ONE) * U256::from(10_000_u32) / self.estimate)
            .saturating_to::<u16>()
            .min(GAS_SHARE_LOOSE_BPS)
    }

    /// The knob's position at a share, in basis points of the bar from its start.
    pub(super) fn position_bps(&self, share_bps: u16) -> u16 {
        if self.span.is_zero() {
            return 10_000;
        }
        let allowance =
            self.estimate.saturating_mul(U256::from(share_bps)) / U256::from(10_000_u32);
        let from_end = (allowance.saturating_mul(U256::from(10_000_u32)) / self.span)
            .min(U256::from(10_000_u32))
            .saturating_to::<u16>();
        10_000 - from_end
    }

    /// The share at a position in basis points of the bar from its start, to the nearest
    /// basis point, and at most the largest share with a positive minimum.
    pub(super) fn share_at(&self, position_bps: u16) -> u16 {
        if self.estimate.is_zero() {
            return 0;
        }
        let from_end = U256::from(10_000_u16.saturating_sub(position_bps));
        // The allowance, `span * from_end / 10000`, as basis points of the estimate.
        let share =
            (self.span.saturating_mul(from_end) + self.estimate / U256::from(2)) / self.estimate;
        share.saturating_to::<u16>().min(self.max_share_bps())
    }

    /// The share whose minimum before the shield fee is `pre_fee`, clamped to the bar.
    pub(super) fn share_for_pre_fee(&self, pre_fee: U256) -> u16 {
        if self.estimate.is_zero() {
            return 0;
        }
        let allowance = self.tolerated.saturating_sub(pre_fee);
        let share = (allowance.saturating_mul(U256::from(10_000_u32))
            + self.estimate / U256::from(2))
            / self.estimate;
        share.saturating_to::<u16>().min(self.max_share_bps())
    }
}

/// Converts the order's amounts, in the bought token, to the amounts the gas strip shows: for
/// a Bridge swap, its destination token, at the quote's ratio of the minimum received there to
/// the deposit. A private delivery receives its destination minimum less the shield fee there.
/// That ratio holds until the bridge quote a share change asks for.
#[derive(Clone, Copy)]
pub(super) struct StripScale {
    pub(super) shown: U256,
    pub(super) ordered: U256,
}

impl StripScale {
    pub(super) fn of(review: &SwapReview) -> Self {
        match review.bridge() {
            Some(bridge) if !review.suggested_private_minimum().is_zero() => Self {
                shown: bridge.received_minimum(),
                ordered: review.suggested_private_minimum(),
            },
            _ => Self {
                shown: U256::ONE,
                ordered: U256::ONE,
            },
        }
    }

    pub(super) fn show(self, ordered: U256) -> U256 {
        ordered.saturating_mul(self.shown) / self.ordered
    }

    pub(super) fn order(self, shown: U256) -> U256 {
        if self.shown.is_zero() {
            return U256::ZERO;
        }
        shown.saturating_mul(self.ordered) / self.shown
    }
}

/// Retry for a Bridge swap's routes, which asks `view`'s providers again.
pub(super) fn bridge_routes_retry_button(view: Entity<PrivateSwapsView>, disabled: bool) -> Button {
    app_button("swap-bridge-routes-retry", "Retry")
        .debug_selector(|| "swap-bridge-routes-retry".into())
        .outline()
        .small()
        .flex_none()
        .disabled(disabled)
        .on_click(move |_, window, cx| {
            view.update(cx, |view, cx| view.retry_bridge_routes(window, cx));
        })
}

/// The unreachable provider notice as a hint card.
pub(super) fn bridge_notice_card(provider: &'static str, window: &Window) -> gpui::Div {
    hint_card("Some tokens may be missing", theme::WARNING, window).child(
        div().whitespace_normal().child(format!(
            "{provider} is unreachable, so tokens only it delivers aren't listed."
        )),
    )
}

/// A provider's hint card: its `notes`, then its `disclaimer`. NEAR Intents' title is a warning.
pub(super) fn provider_hint_card(
    provider: BridgeProvider,
    notes: &[SharedString],
    disclaimer: &SharedString,
    window: &Window,
) -> gpui::Div {
    let warning = provider == BridgeProvider::NearIntents;
    let title_color = if warning { theme::WARNING } else { theme::INFO };
    hint_card(provider_name(provider), title_color, window)
        .children(
            notes
                .iter()
                .map(|note| div().whitespace_normal().child(note.clone())),
        )
        .child(div().whitespace_normal().child(disclaimer.clone()))
}

/// The gas explanation as a hint card: `text`'s paragraphs under the title, with a legend of
/// the bar's graphics in the `(solid, muted)` colours while the bar shows.
pub(super) fn gas_help_card(
    text: &[String; 5],
    legend: Option<(gpui::Hsla, gpui::Hsla)>,
    window: &Window,
) -> gpui::Div {
    let [costs, gas, solvers, minimum, fees] = text;
    let paragraph = |text: &String| div().whitespace_normal().child(text.clone());
    let entry = |marker: gpui::Div, label: &'static str| {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w_4()
                    .flex_none()
                    .flex()
                    .justify_center()
                    .child(marker),
            )
            .child(div().text_color(rgb(theme::TEXT_MUTED)).child(label))
    };
    hint_card(GAS_HELP_TITLE, theme::INFO, window)
        .w(rems(20.))
        .children([costs, gas, solvers, minimum].map(paragraph))
        .children(legend.map(|(solid, muted)| {
            div()
                .debug_selector(|| "swap-gas-help-legend".into())
                .flex()
                .flex_col()
                .gap_1()
                .child(entry(
                    div().size_3().rounded_full().bg(solid),
                    "Your minimum",
                ))
                .child(entry(
                    div().w_4().h_1().rounded_full().bg(solid),
                    "Where the swap can end up",
                ))
                .child(entry(
                    div()
                        .w_4()
                        .h_0()
                        .border_t_2()
                        .border_dashed()
                        .border_color(muted),
                    "Losses you've refused",
                ))
        }))
        .child(paragraph(fees))
}

/// The slider value's nearest whole percent.
pub(super) fn slider_percent(value: f32) -> u16 {
    (0..=100_u16)
        .min_by(|left, right| {
            (f32::from(*left) - value)
                .abs()
                .total_cmp(&(f32::from(*right) - value).abs())
        })
        .unwrap_or(0)
}

/// A position in basis points of the gas bar at the bar's nearest step, in percent.
pub(super) const fn nearest_step(position_bps: u16) -> u16 {
    let step = GAS_BAR_STEP * 100;
    (position_bps + step / 2) / step * GAS_BAR_STEP
}

/// An RPC gas price in gwei, "1.18": two decimals from 1 gwei and three below it, without
/// trailing zeros.
pub(in crate::root::private_swap) fn format_gwei(wei: u128) -> String {
    let (unit, divisor, width) = if wei >= 1_000_000_000 {
        (10_000_000_u128, 100_u128, 2_usize)
    } else {
        (1_000_000, 1_000, 3)
    };
    let scaled = wei.saturating_add(unit / 2) / unit;
    if scaled == 0 {
        return if wei == 0 { "0" } else { "< 0.001" }.to_owned();
    }
    let text = format!("{}.{:0width$}", scaled / divisor, scaled % divisor);
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The name of a gas share: its preset, or Custom with the share.
pub(in crate::root::private_swap) fn gas_share_name(share_bps: u16) -> String {
    GasPreset::of_share(share_bps).map_or_else(
        || format!("Custom · {}", format_bps_percent(u64::from(share_bps))),
        |preset| preset.summary().to_owned(),
    )
}

/// Expected value retained in buy-token units after the allowed gas, shield and bridge fees.
/// Across includes both the destination payment and source-chain return.
pub(super) fn expected_output(review: &SwapReview) -> U256 {
    let quoted = expected_payout(review);
    quoted
        .saturating_sub(shield_fee_on(review, quoted))
        .saturating_sub(bridge_cost(review))
}

/// The `CoW` payout if solvers charge the allowed gas, before delivery.
pub(super) const fn expected_payout(review: &SwapReview) -> U256 {
    review.best_case().saturating_sub(review.gas_allowance())
}

/// The bridge fee in buy-token base units, zero without a bridge or a priced leg.
pub(super) fn bridge_fee(review: &SwapReview) -> U256 {
    review
        .bridge()
        .and_then(|bridge| bridge.fee)
        .unwrap_or_default()
}

/// Provider fees plus private destination gas and shielding, in buy-token base units.
pub(super) fn bridge_cost(review: &SwapReview) -> U256 {
    bridge_fee(review).saturating_add(private_delivery_cost(review))
}

/// Value the private delivery's deductions at Across's same-asset quote ratio. The source
/// deposit less the provider fee corresponds to `quoted_output` before destination costs;
/// scaling the net credit avoids assuming equal token decimals on the two chains.
pub(super) fn private_delivery_cost(review: &SwapReview) -> U256 {
    let Some((bridge, private)) = review
        .bridge()
        .and_then(|bridge| bridge.private.map(|private| (bridge, private)))
    else {
        return U256::ZERO;
    };
    let before_delivery = review
        .suggested_private_minimum()
        .saturating_sub(bridge_fee(review));
    let retained = before_delivery
        .saturating_mul(bridge.received_minimum())
        .checked_div(private.quoted_output)
        .unwrap_or_default();
    before_delivery.saturating_sub(retained)
}

/// Railgun's shield fee on a buy-token amount.
pub(super) fn shield_fee_on(review: &SwapReview, amount: U256) -> U256 {
    review.shield_fee_on_output(amount)
}
