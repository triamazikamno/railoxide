//! A Bridge swap's second leg: the provider clients on the swap's network route, the bridge
//! quote for the order's buy amount, which runs after the `CoW` quote, and the provider terms
//! quoted again while signing.
//!
//! Review quotes never carry the receiver or the executor: Across fee quotes name only tokens,
//! chains and the amount, and 1Click dry quotes send random placeholders. Only the 1Click quote
//! for an approved order that is being signed names them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, Bytes, U256, U512};
use broadcaster_core::contracts::across::SpokePool;
use eyre::{Result, eyre};
use reqwest::Url;

use super::order::anchor_token;
use super::{SwapInputPlan, SwapReview, SwapReviewChange};
use crate::bridge::{
    AcrossClient, AcrossFeeQuote, AcrossFeeRequest, BridgeDestination, NearIntentsClient,
    OneClickDryQuote, OneClickQuoteParams,
};
use crate::cow::CowOrderbookClient;
use crate::settings::{EffectiveChainConfig, EffectiveTokenRegistry, SwapProfile};
use crate::vault::{
    AcrossOrderTerms, BridgeDelivery, BridgeOrderTerms, BridgeProvider, NearIntentsOrderTerms,
    SwapDelivery,
};
use crate::{
    ExecutorOwner, PairAnchorRate, QuoteDeviationError, TokenAnchorRateCache,
    check_quote_against_anchor,
};

/// How long a 1Click quote outlives the order's validity.
const NEAR_QUOTE_DEADLINE_MARGIN: Duration = Duration::from_hours(1);
/// How long a relayer can still fill an Across deposit after its order expires.
const ACROSS_FILL_MARGIN_SECS: u64 = 30 * 60;
/// The `SpokePool`'s `fillDeadlineBuffer`: a deposit's fill deadline is at most this far past
/// its block's timestamp.
const ACROSS_FILL_DEADLINE_BUFFER_SECS: u64 = 21_600;
/// The `SpokePool`'s `depositQuoteTimeBuffer`: a deposit's quote timestamp is at most this old
/// at its block.
const ACROSS_QUOTE_TIME_BUFFER_SECS: u64 = 3_600;

/// Bridge provider clients on one swap's network route, the isolation group of its orderbook
/// client.
#[derive(Clone, Debug)]
pub struct SwapBridgeClients {
    pub across: AcrossClient,
    pub near: NearIntentsClient,
}

/// A Bridge delivery's route, from its provider's destination list, for the review to quote.
#[derive(Clone, Copy)]
pub struct SwapBridgeRoute<'a> {
    pub clients: &'a SwapBridgeClients,
    pub destination: &'a BridgeDestination,
    pub destination_chain: &'a EffectiveChainConfig,
}

/// A Bridge order's provider terms quoted while signing, or the term that changed since the
/// review. Nothing is signed for a change.
pub(super) enum BridgeSigning {
    Terms(BridgeOrderTerms),
    Changed(SwapReviewChange),
}

/// How the bridge leg's rate was checked at review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeLegPrice {
    /// The provider delivers the bought token's own asset, so its fee is the leg's whole cost.
    SameAsset,
    /// The leg's minimum output is within the profile's deviation from both tokens' anchors.
    Verified(PairAnchorRate),
    /// Different assets without usable anchors. Approval requires the user's acknowledgement.
    Unverified,
}

/// The bridge leg of a Bridge review, quoted for the order's buy amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapBridgeQuote {
    pub provider: BridgeProvider,
    /// The minimum received on the destination chain, which the approval binds.
    pub destination_minimum: U256,
    /// The provider's expected output on the destination chain.
    pub expected_output: U256,
    /// The bridge's cost in bought-token base units. `None` for an unverified leg between
    /// different assets.
    pub fee: Option<U256>,
    pub leg: BridgeLegPrice,
    pub fill_time_sec: Option<u64>,
}

impl ExecutorOwner {
    /// Bridge clients on the route of the swap `orderbook` belongs to. Keep them with it.
    pub fn swap_bridge_clients(&self, orderbook: &CowOrderbookClient) -> Result<SwapBridgeClients> {
        let profile = self
            .chain
            .bridge_profile()
            .ok_or_else(|| eyre!("this network doesn't support bridging"))?;
        let http = orderbook.http();
        Ok(SwapBridgeClients {
            across: AcrossClient::new(http.clone(), Url::parse(profile.across_api_base())?)?,
            near: NearIntentsClient::new(
                http.clone(),
                Url::parse(profile.one_click_api_base())?,
                profile.one_click_quote_key(),
            )?,
        })
    }

    /// Quote `route`'s provider for `buy_amount` of the bought token, the order's buy amount.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn quote_swap_bridge(
        &self,
        route: SwapBridgeRoute<'_>,
        delivery: BridgeDelivery,
        profile: &SwapProfile,
        buy_amount: U256,
        slippage_bps: u32,
        anchor_cache: Option<&TokenAnchorRateCache>,
        token_registry: &EffectiveTokenRegistry,
    ) -> Result<SwapBridgeQuote> {
        let destination = route.destination;
        let quote = match delivery.provider {
            BridgeProvider::Across => {
                let spoke_pool = self
                    .chain
                    .bridge_profile()
                    .ok_or_else(|| eyre!("this network doesn't support bridging"))?
                    .spoke_pool();
                let fees = route
                    .clients
                    .across
                    .suggested_fees(&AcrossFeeRequest {
                        input_token: destination.intermediate,
                        output_token: delivery.destination_token,
                        origin_chain: self.chain.chain_id,
                        destination_chain: delivery.destination_chain,
                        amount: buy_amount,
                    })
                    .await?;
                across_bridge_quote(&fees, spoke_pool)?
            }
            BridgeProvider::NearIntents => {
                let assets = destination
                    .near
                    .as_ref()
                    .ok_or_else(|| eyre!("NEAR Intents doesn't list this destination"))?;
                let deadline = SystemTime::now()
                    .checked_add(profile.valid_to_window() + NEAR_QUOTE_DEADLINE_MARGIN)
                    .ok_or_else(|| eyre!("the bridge quote's deadline is out of range"))?;
                let quote = route
                    .clients
                    .near
                    .dry_quote(&OneClickQuoteParams {
                        origin_asset: assets.origin_asset.clone(),
                        destination_asset: assets.destination_asset.clone(),
                        amount: buy_amount,
                        slippage_bps,
                        deadline,
                    })
                    .await?;
                let anchor = anchor_cache.and_then(|cache| {
                    cache
                        .cached_bridge_leg_rate(
                            self.chain.chain_id,
                            destination.intermediate,
                            delivery.destination_chain,
                            anchor_token(delivery.destination_chain, delivery.destination_token),
                            token_registry,
                        )
                        .ok()
                        .flatten()
                });
                near_bridge_quote(
                    buy_amount,
                    &quote,
                    anchor,
                    destination,
                    profile.anchor_deviation_bps(),
                )?
            }
        };
        if quote.destination_minimum.is_zero() {
            return Err(eyre!("nothing is left to receive after the bridge fee"));
        }
        Ok(quote)
    }

    /// Quote `route`'s provider again for `review`'s approved order: `buy_amount` of the bought
    /// token, from an order valid until `valid_to`, for at least `destination_minimum` on the
    /// destination chain. A 1Click quote names the receiver and the executor, so it isn't
    /// requested when a leg verified at review can no longer be priced.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn bridge_signing_terms(
        &self,
        review: &SwapReview,
        route: SwapBridgeRoute<'_>,
        delivery: BridgeDelivery,
        buy_amount: U256,
        destination_minimum: U256,
        valid_to: u32,
        profile: &SwapProfile,
        anchor_cache: &TokenAnchorRateCache,
        token_registry: &EffectiveTokenRegistry,
    ) -> Result<BridgeSigning> {
        let plan = review.plan();
        let destination = route.destination;
        match delivery.provider {
            BridgeProvider::Across => {
                let spoke_pool = self
                    .chain
                    .bridge_profile()
                    .ok_or_else(|| eyre!("this network doesn't support bridging"))?
                    .spoke_pool();
                let fees = route
                    .clients
                    .across
                    .suggested_fees(&AcrossFeeRequest {
                        input_token: plan.buy_token(),
                        output_token: delivery.destination_token,
                        origin_chain: self.chain.chain_id,
                        destination_chain: delivery.destination_chain,
                        amount: buy_amount,
                    })
                    .await?;
                across_order_terms(
                    &fees,
                    spoke_pool,
                    plan.buy_token(),
                    delivery,
                    buy_amount,
                    destination_minimum,
                    valid_to,
                )
            }
            BridgeProvider::NearIntents => {
                let assets = destination
                    .near
                    .as_ref()
                    .ok_or_else(|| eyre!("NEAR Intents doesn't list this destination"))?;
                let anchor = anchor_cache
                    .cached_bridge_leg_rate(
                        self.chain.chain_id,
                        plan.buy_token(),
                        delivery.destination_chain,
                        anchor_token(delivery.destination_chain, delivery.destination_token),
                        token_registry,
                    )
                    .ok()
                    .flatten();
                if anchor.is_none()
                    && review
                        .bridge()
                        .is_some_and(|bridge| matches!(bridge.leg, BridgeLegPrice::Verified(_)))
                {
                    return Ok(BridgeSigning::Changed(SwapReviewChange::PriceUnavailable));
                }
                let deadline = UNIX_EPOCH
                    .checked_add(
                        Duration::from_secs(u64::from(valid_to)) + NEAR_QUOTE_DEADLINE_MARGIN,
                    )
                    .ok_or_else(|| eyre!("the bridge quote's deadline is out of range"))?;
                let executor = plan.executor();
                // Refunds return to the executor on this chain.
                let quote = route
                    .clients
                    .near
                    .quote(
                        &OneClickQuoteParams {
                            origin_asset: assets.origin_asset.clone(),
                            destination_asset: assets.destination_asset.clone(),
                            amount: buy_amount,
                            slippage_bps: review.slippage_bps(),
                            deadline,
                        },
                        delivery.receiver,
                        executor,
                        executor,
                    )
                    .await?;
                if quote.min_amount_out < destination_minimum {
                    return Ok(BridgeSigning::Changed(
                        SwapReviewChange::DestinationMinimum {
                            approved: destination_minimum,
                            current: quote.min_amount_out,
                        },
                    ));
                }
                if let Some(rate) = anchor {
                    match check_quote_against_anchor(
                        buy_amount,
                        quote.min_amount_out,
                        rate,
                        profile.anchor_deviation_bps(),
                    ) {
                        Ok(()) => {}
                        Err(QuoteDeviationError::ExceedsThreshold) => {
                            return Ok(BridgeSigning::Changed(SwapReviewChange::QuoteDeviates));
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(BridgeSigning::Terms(BridgeOrderTerms::NearIntents(
                    NearIntentsOrderTerms {
                        deposit_address: quote.deposit_address,
                        min_amount_out: quote.min_amount_out,
                        amount_out: quote.amount_out,
                        deadline: quote.deadline,
                        signed_quote: quote.signed_response,
                    },
                )))
            }
        }
    }
}

/// The validated route of `plan`'s Bridge delivery, which requires one; other deliveries take
/// none. The route must bridge the plan's bought token to the delivery's token and chain with
/// the delivery's provider.
pub(super) fn bridge_route<'a>(
    plan: &SwapInputPlan,
    route: Option<SwapBridgeRoute<'a>>,
) -> Result<Option<(BridgeDelivery, SwapBridgeRoute<'a>)>> {
    match (plan.delivery(), route) {
        (SwapDelivery::Bridge(delivery), Some(route)) => {
            let destination = route.destination;
            if destination.intermediate != plan.buy_token()
                || destination.destination_token != delivery.destination_token
                || route.destination_chain.chain_id != delivery.destination_chain
                || (delivery.provider == BridgeProvider::Across) != destination.near.is_none()
            {
                return Err(eyre!("the bridge route doesn't match the swap's delivery"));
            }
            Ok(Some((delivery, route)))
        }
        (SwapDelivery::Bridge(_), None) => Err(eyre!("a bridge swap needs its bridge route")),
        (_, Some(_)) => Err(eyre!("only a bridge swap takes a bridge route")),
        (_, None) => Ok(None),
    }
}

/// An Across fee quote for the order's buy amount. Its output is the destination minimum and
/// its relay fee, which includes the LP fee, the bridge fee. The quote must name the swap
/// chain's `SpokePool`: the wallet never takes the deposit contract from the API.
pub(super) fn across_bridge_quote(
    fees: &AcrossFeeQuote,
    spoke_pool: Address,
) -> Result<SwapBridgeQuote> {
    if fees.spoke_pool != spoke_pool {
        return Err(eyre!("Across quoted a deposit into an unknown SpokePool"));
    }
    Ok(SwapBridgeQuote {
        provider: BridgeProvider::Across,
        destination_minimum: fees.output_amount,
        expected_output: fees.output_amount,
        fee: Some(fees.total_relay_fee_total),
        leg: BridgeLegPrice::SameAsset,
        fill_time_sec: Some(fees.estimated_fill_time_sec),
    })
}

/// The deposit terms of a fresh Across quote for an approved order: `buy_amount` of
/// `input_token` for the approved `destination_minimum`, not the quote's own output. The quote
/// must name the swap chain's `SpokePool`. Its fill deadline must leave relayers 30 minutes
/// after the order expires, and its timestamp and fill deadline must pass the `SpokePool`'s
/// buffers for any settlement before `valid_to`: a deposit's block can't precede its quote.
fn across_order_terms(
    fees: &AcrossFeeQuote,
    spoke_pool: Address,
    input_token: Address,
    delivery: BridgeDelivery,
    buy_amount: U256,
    destination_minimum: U256,
    valid_to: u32,
) -> Result<BridgeSigning> {
    let quote = across_bridge_quote(fees, spoke_pool)?;
    if quote.destination_minimum < destination_minimum {
        return Ok(BridgeSigning::Changed(
            SwapReviewChange::DestinationMinimum {
                approved: destination_minimum,
                current: quote.destination_minimum,
            },
        ));
    }
    let timestamp = u64::from(fees.timestamp);
    let fill_deadline = u64::from(fees.fill_deadline);
    let valid_to = u64::from(valid_to);
    if fill_deadline < valid_to + ACROSS_FILL_MARGIN_SECS
        || fill_deadline > timestamp + ACROSS_FILL_DEADLINE_BUFFER_SECS
        || timestamp + ACROSS_QUOTE_TIME_BUFFER_SECS < valid_to
    {
        return Err(eyre!(
            "Across's quote can't be used for this order; try again"
        ));
    }
    Ok(BridgeSigning::Terms(BridgeOrderTerms::Across(
        AcrossOrderTerms {
            spoke_pool,
            input_token,
            output_token: delivery.destination_token,
            input_amount: buy_amount,
            output_amount: destination_minimum,
            quote_timestamp: fees.timestamp,
            fill_deadline: fees.fill_deadline,
            exclusive_relayer: fees.exclusive_relayer,
            exclusivity_parameter: fees.exclusivity_deadline,
        },
    )))
}

/// The `depositV3` call of an Across order's post-hook: `terms`, deposited by the executor for
/// `delivery`'s receiver with an empty message.
pub(super) fn across_deposit(
    executor: Address,
    delivery: BridgeDelivery,
    terms: &AcrossOrderTerms,
) -> SpokePool::depositV3Call {
    SpokePool::depositV3Call {
        depositor: executor,
        recipient: delivery.receiver,
        inputToken: terms.input_token,
        outputToken: terms.output_token,
        inputAmount: terms.input_amount,
        outputAmount: terms.output_amount,
        destinationChainId: U256::from(delivery.destination_chain),
        exclusiveRelayer: terms.exclusive_relayer,
        quoteTimestamp: terms.quote_timestamp,
        fillDeadline: terms.fill_deadline,
        exclusivityParameter: terms.exclusivity_parameter,
        message: Bytes::new(),
    }
}

/// A 1Click dry quote for `buy_amount` of `destination`'s intermediate. Its minimum output is
/// the destination minimum. With an `anchor` rate for the leg, that minimum must be within
/// `deviation_bps` of it, and the fee is `buy_amount` less the minimum's value in the
/// intermediate. Without one, a same-asset leg's fee is `buy_amount` less the minimum in the
/// intermediate's decimals, and a leg between different assets is unverified.
pub(super) fn near_bridge_quote(
    buy_amount: U256,
    quote: &OneClickDryQuote,
    anchor: Option<PairAnchorRate>,
    destination: &BridgeDestination,
    deviation_bps: u32,
) -> Result<SwapBridgeQuote> {
    let minimum = quote.min_amount_out;
    let (fee, leg) = match (anchor, &destination.near) {
        (Some(rate), _) => {
            check_quote_against_anchor(buy_amount, minimum, rate, deviation_bps)?;
            let value =
                U512::from(minimum) * U512::from(rate.sell_rate) / U512::from(rate.buy_rate);
            (
                Some(buy_amount.saturating_sub(U256::saturating_from(value))),
                BridgeLegPrice::Verified(rate),
            )
        }
        (None, Some(assets)) if destination.same_asset => (
            Some(buy_amount.saturating_sub(rescale(
                minimum,
                assets.destination_decimals,
                assets.origin_decimals,
            ))),
            BridgeLegPrice::SameAsset,
        ),
        (None, _) => (None, BridgeLegPrice::Unverified),
    };
    Ok(SwapBridgeQuote {
        provider: BridgeProvider::NearIntents,
        destination_minimum: minimum,
        expected_output: quote.amount_out,
        fee,
        leg,
        fill_time_sec: quote.time_estimate_sec,
    })
}

/// `amount` in `from` decimals, in `to` decimals, rounded down.
fn rescale(amount: U256, from: u8, to: u8) -> U256 {
    let scale = U256::from(10_u8).checked_pow(U256::from(from.abs_diff(to)));
    if to >= from {
        scale.map_or(U256::MAX, |scale| amount.saturating_mul(scale))
    } else {
        scale.map_or(U256::ZERO, |scale| amount / scale)
    }
}
