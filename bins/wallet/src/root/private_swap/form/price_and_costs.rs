#[allow(clippy::wildcard_imports)]
use super::*;

/// The quote against the anchor price, and the reading behind it.
pub(super) struct PriceDelta {
    /// In basis points. None when the amounts give no ratio.
    bps: Option<i64>,
    /// "Chainlink" only when every anchor reading came from a Chainlink round.
    pub(super) anchor: &'static str,
    pub(super) checked: Option<String>,
}

impl PriceDelta {
    /// The quote is below the anchor, as shown: `format_bps_percent` rounds nothing away.
    pub(super) fn below(&self) -> bool {
        self.bps.is_some_and(|bps| bps < 0)
    }

    /// "−0.4%" in the color of its sign, or that the quote is within range.
    pub(super) fn value(&self, cx: &App) -> gpui::Div {
        let Some(bps) = self.bps else {
            return app_muted_text("within range");
        };
        let color = match bps.cmp(&0) {
            std::cmp::Ordering::Less => cx.theme().danger,
            std::cmp::Ordering::Equal => cx.theme().muted_foreground,
            std::cmp::Ordering::Greater => cx.theme().success,
        };
        app_text(format!(
            "{}{}",
            if bps < 0 { "−" } else { "+" },
            format_bps_percent(bps.unsigned_abs())
        ))
        .text_color(color)
    }
}

/// A price below its anchor, in parentheses after the amounts it belongs to. Its tooltip
/// says what it was checked against.
pub(super) fn price_delta_note(delta: PriceDelta, cx: &App) -> gpui::Stateful<gpui::Div> {
    div()
        .id("swap-price-delta")
        .flex()
        .items_baseline()
        .child(app_muted_text("("))
        .child(delta.value(cx))
        .child(app_muted_text(format!(" vs {})", delta.anchor)))
        .when_some(delta.checked, |delta, checked| {
            delta.tooltip(move |window, cx| Tooltip::new(checked.clone()).build(window, cx))
        })
}

pub(super) fn price_delta(review: &SwapReview) -> Option<PriceDelta> {
    let quote = review.quote();
    checked_price_delta(review.price(), Some((quote.sell_amount, quote.buy_amount)))
}

/// A checked `price` against its anchor, for the `quoted` sell and buy amounts. Without them
/// the delta has no figure and reads as within range, which a verified price is.
pub(super) fn checked_price_delta(
    price: &SwapPrice,
    quoted: Option<(U256, U256)>,
) -> Option<PriceDelta> {
    let SwapPrice::Verified { rate, observations } = price else {
        return None;
    };
    let anchor = if !observations.is_empty()
        && observations
            .iter()
            .all(|observation| observation.updated_at.is_some())
    {
        "Chainlink"
    } else {
        "the anchor price"
    };
    let checked = observations
        .iter()
        .map(|observation| observation.block.number)
        .max()
        .map(|block| format!("Checked against {anchor} at block {block}"));
    Some(PriceDelta {
        bps: quoted.and_then(|(sold, bought)| {
            let expected = if rate.sell_rate.is_zero() {
                U256::ZERO
            } else {
                sold.saturating_mul(rate.buy_rate) / rate.sell_rate
            };
            quote_anchor_delta_bps(bought, expected)
        }),
        anchor,
        checked,
    })
}

/// Whether a USD value reads as the amount beside it, as "≈ $10.00" does beside "10": the same
/// number once "≈", "$", thousands separators and trailing zeros are dropped.
pub(super) fn usd_repeats_amount(usd: &str, amount: &str) -> bool {
    let number = |text: &str| {
        let text = text
            .chars()
            .filter(|symbol| !matches!(*symbol, '≈' | '$' | ',') && !symbol.is_whitespace())
            .collect::<String>();
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            text
        }
    };
    number(usd) == number(amount)
}

/// The Returned row's hint: where the surplus above a Bridge order's `deposit` goes on `chain`,
/// what comes off it, and that there may be none.
pub(super) fn source_return_hint(review: &SwapReview, deposit: &str, chain: &str) -> String {
    match review.plan().delivery() {
        SwapDelivery::Bridge(BridgeDelivery {
            surplus: BridgeSurplus::Reshield,
            ..
        }) => format!(
            "Anything CoW pays above the {deposit} deposit is reshielded to your private balance, after estimated gas and the shield fee. It may be zero."
        ),
        _ => format!(
            "Anything CoW pays above the {deposit} deposit is kept in your stealth account on {chain}, after estimated gas. It may be zero."
        ),
    }
}

/// What one whole sold token buys, in the bought token's base units, when `sell_amount` of a
/// token with `sell_decimals` is exchanged for `buy_amount`. `None` for no sold amount.
pub(super) fn unit_rate(sell_amount: U256, sell_decimals: u8, buy_amount: U256) -> Option<U256> {
    if sell_amount.is_zero() {
        return None;
    }
    Some(buy_amount.saturating_mul(U256::from(10u8).pow(U256::from(sell_decimals))) / sell_amount)
}

/// The Bridge row's value for Across: the provider, its `fee`, and a private delivery's
/// `allowance` for the shield's gas, without its symbol.
pub(super) fn across_row_value(
    provider: &str,
    fee: Option<&str>,
    allowance: Option<&str>,
) -> String {
    match (fee, allowance) {
        (Some(fee), Some(allowance)) => format!("{provider} · {fee} + ≈ {allowance} delivery"),
        (Some(fee), None) => format!("{provider} · fee {fee}"),
        (None, _) => provider.to_owned(),
    }
}

/// What a private delivery through Across costs, for the Bridge row's hint: its `fee` and the
/// `allowance` for the shield's gas on `network`.
pub(super) fn across_delivery_note(fee: &str, network: u64, allowance: &str) -> String {
    format!(
        "Across charges {fee}. Shielding on delivery costs the relayer extra gas on {}, estimated at {allowance}.",
        network_name(network)
    )
}

/// A new swap's steps, as its reviews' stepper names them. A private Bridge swap sets up
/// `two_accounts`.
pub(in crate::root::private_swap) const fn swap_steps(two_accounts: bool) -> [&'static str; 2] {
    if two_accounts {
        PRIVATE_BRIDGE_SWAP_STEPS
    } else {
        SWAP_STEPS
    }
}

/// The stepper's explanation of a new swap's two steps.
pub(in crate::root::private_swap) fn swap_steps_hint(two_accounts: bool) -> SpendAuthorizationHint {
    let setup = if two_accounts {
        "1. Set up stealth accounts. A broadcaster on each network creates a one-time account for this swap, and you pay both setup fees."
    } else {
        "1. Set up stealth account. A broadcaster creates a one-time account for this swap, and you pay the setup fee."
    };
    SpendAuthorizationHint::new(
        "Two steps",
        [
            setup,
            "2. Place order. Once setup confirms, the wallet checks the quote again and asks you to confirm. If the terms changed, you review them first.",
            "With \"Just this spend\", you enter the password again for step 2.",
        ],
    )
}

/// A failure choice as its control and the review name it, after the network the funds end up
/// on: the swap's `origin`, or the `destination`.
pub(super) fn shield_failure_label(
    choice: BridgeShieldFailure,
    origin: &str,
    destination: &str,
) -> String {
    match choice {
        BridgeShieldFailure::RefundOnOrigin => format!("Refund on {origin}"),
        BridgeShieldFailure::KeepOnDestination => format!("Keep on {destination}"),
    }
}

/// [`shield_failure_label`] as a review row's value, which carries the network's mark.
pub(super) fn shield_failure_mention(
    choice: BridgeShieldFailure,
    origin: u64,
    destination: u64,
) -> SpendAuthorizationLabel {
    match choice {
        BridgeShieldFailure::RefundOnOrigin => {
            SpendAuthorizationLabel::on_network("Refund on", origin, "")
        }
        BridgeShieldFailure::KeepOnDestination => {
            SpendAuthorizationLabel::on_network("Keep on", destination, "")
        }
    }
}

/// The info icon beside the estimated return, whose hover says where the return goes.
pub(super) fn source_return_help(review: &SwapReview) -> gpui::Stateful<gpui::Div> {
    let note = source_return_note(review);
    div()
        .id("swap-source-return-help")
        .flex_none()
        .tooltip(move |window, cx| {
            Tooltip::element(move |window, _| {
                hint_card("Estimated return", theme::INFO, window)
                    .w(rems(20.))
                    .child(div().whitespace_normal().child(note))
            })
            .build(window, cx)
        })
        .child(
            app_button_base("swap-source-return-help-trigger")
                .ghost()
                .xsmall()
                .icon(IconName::Info)
                .accessibility_label(note)
                .debug_selector(|| "swap-source-return-help".into()),
        )
}

fn source_return_note(review: &SwapReview) -> &'static str {
    match review.plan().delivery() {
        SwapDelivery::Bridge(BridgeDelivery {
            surplus: BridgeSurplus::Reshield,
            ..
        }) => {
            "To your private balance, after estimated gas and the shield fee. Surplus may be zero."
        }
        _ => "Kept in your stealth account, after estimated gas. Surplus may be zero.",
    }
}

/// A review's collapsed Costs line: the `total` as the form's costs line has it, then
/// `pay_now`, the amounts paid up front and not refunded.
pub(super) fn review_costs_summary(total: Option<String>, pay_now: Option<&str>) -> String {
    total
        .into_iter()
        .chain(pay_now.map(|amounts| format!("up to {amounts} now, not refunded")))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Estimated swap fees, including hook gas, at the quoted trading rate. Setup is separate.
pub(super) fn total_cost(review: &SwapReview) -> U256 {
    swap_fees_to(review, expected_output(review))
}

/// The unshield fee, converted at the best-case rate, and every deduction from the best case
/// down to `received`. The best case already counts `CoW`'s network fee as output.
pub(super) fn swap_fees_to(review: &SwapReview, received: U256) -> U256 {
    let unshield_fee = review.plan().amount().saturating_sub(review.sell_amount());
    swap_total_cost(
        unshield_fee,
        review
            .quote()
            .sell_amount
            .saturating_add(review.quote().fee_amount),
        review.best_case(),
        received,
    )
}

/// Costs the user authorizes, in basis points of the swap: the swap amount less the
/// `minimum`, both in the Buy token at the quoted trading rate. The swap amount is the `best`
/// case of the order's `sold` amount, scaled to the `spent` amount before the unshield fee.
pub(super) fn authorized_cost_bps(best: U256, spent: U256, sold: U256, minimum: U256) -> u64 {
    if sold.is_zero() {
        return 0;
    }
    let swap = best.saturating_mul(spent) / sold;
    if swap.is_zero() {
        return 0;
    }
    (swap
        .saturating_sub(minimum)
        .saturating_mul(U256::from(10_000_u32))
        / swap)
        .saturating_to::<u64>()
}

/// A review's authorized costs, when they are high enough to need Swap anyway. A Bridge
/// swap's minimum is its destination credit, valued as the deposit less all delivery costs.
pub(super) fn authorized_high_cost(review: &SwapReview) -> Option<u64> {
    let minimum = review
        .suggested_private_minimum()
        .saturating_sub(bridge_cost(review));
    let bps = authorized_cost_bps(
        review.best_case(),
        review.plan().amount(),
        review.sell_amount(),
        minimum,
    );
    (bps >= AUTHORIZED_COST_WARNING_BPS).then_some(bps)
}

/// The review the gas strip shows: the quote at the form's share. A Bridge swap prices a new
/// share locally until its bridge leg is quoted again. A share the quote can't support shows
/// the quote as it was priced, at Tight.
pub(super) fn strip_review(form: &SwapForm, review: &Arc<SwapReview>) -> Arc<SwapReview> {
    if review.gas_share_bps() == form.gas_share_bps {
        return Arc::clone(review);
    }
    review
        .with_gas_share(form.gas_share_bps)
        .map_or_else(|_| Arc::clone(review), Arc::new)
}

/// The best case less Railgun's fee on the output: the shield fee, for Private delivery.
pub(super) fn best_after_fees(review: &SwapReview) -> U256 {
    let best = review.best_case();
    match review.plan().delivery() {
        SwapDelivery::Reshield => best.saturating_sub(review.shield_fee_on_output(best)),
        _ => best,
    }
}
