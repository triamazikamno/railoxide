//! A Bridge swap's second leg: the provider clients on the swap's network route, the bridge
//! quote for the order's buy amount, which runs after the `CoW` quote, and the provider terms
//! quoted again while signing.
//!
//! Review quotes never carry the receiver or the executor: Across fee quotes name only tokens,
//! chains and the amount, and 1Click dry quotes send random placeholders. Only the 1Click quote
//! for an approved order that is being signed names them, and so does the Across quote for an
//! approved private delivery, which carries the handler and its message. A private delivery's
//! review also reads the destination chain's gas price, and on a rollup its data price, through
//! that chain's RPC route. Neither read names an account.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256, Bytes, U256, U512, keccak256};
use broadcaster_core::contracts::across::SpokePool;
use eyre::{Result, eyre};
use railgun_wallet::tx::{GasEstimateMode, RailgunGasModel};
use reqwest::Url;

use super::gas::hook_data_cost_from_candidates;
use super::order::{anchor_token, placeholder_private_delivery_message_len};
use super::{SwapInputPlan, SwapReview, SwapReviewChange};
use crate::bridge::{
    AcrossClient, AcrossFeeQuote, AcrossFeeRequest, AcrossMessageFeeRequest, BridgeApi,
    BridgeApiError, BridgeDestination, NearAssets, NearIntentsClient, NonEvmBridgeDestination,
    NonEvmNetwork, OneClickDryQuote, OneClickQuoteParams, OneClickRecipient,
};
use crate::cow::{
    CowOrderbookClient, DeliveryAllowanceParams, DeliveryAllowanceRate, OrderLimitError,
    delivery_allowance, destination_minimum_after_allowance, private_delivery_gas,
};
use crate::desktop::{
    acquire_fee_candidates, effective_desktop_chain_config, gas_price_from_candidates_with_policy,
};
use crate::settings::{
    BridgeReceiverRejection, EffectiveChainConfig, EffectiveTokenRegistry, SwapProfile,
};
use crate::vault::{
    AcrossOrderTerms, BridgeDelivery, BridgeOrderTerms, BridgeProvider, NearIntentsOrderTerms,
    NonEvmDelivery, SwapDelivery,
};
use crate::{
    ExecutorOwner, HttpContext, OperationHttpClient, PairAnchorRate, QuoteDeviationError,
    RAILGUN_PROTOCOL_FEE_BPS, TokenAnchorRateCache, check_quote_against_anchor,
    railgun_protocol_fee_amount,
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

impl SwapBridgeClients {
    /// These clients on another network route.
    #[must_use]
    pub fn on_route(&self, http: &OperationHttpClient) -> Self {
        Self {
            across: self.across.on_route(http.clone()),
            near: self.near.on_route(http.clone()),
        }
    }
}

/// A Bridge delivery's route, from its provider's destination list, for the review to quote.
#[derive(Clone, Copy)]
pub struct SwapBridgeRoute<'a> {
    pub clients: &'a SwapBridgeClients,
    pub destination: SwapBridgeDestination<'a>,
}

/// Where a Bridge route delivers.
#[derive(Clone, Copy)]
pub enum SwapBridgeDestination<'a> {
    /// A provider's destination on an EVM chain, with that chain's configuration.
    Chain {
        destination: &'a BridgeDestination,
        chain: &'a EffectiveChainConfig,
    },
    /// A NEAR Intents destination on a non-EVM network, which has no chain configuration.
    NonEvm(&'a NonEvmBridgeDestination),
}

/// A Bridge delivery checked against its route by [`bridge_route`].
pub(super) enum BridgeLeg<'a> {
    Chain {
        delivery: BridgeDelivery,
        destination: &'a BridgeDestination,
        chain: &'a EffectiveChainConfig,
    },
    NonEvm {
        delivery: NonEvmDelivery,
        destination: &'a NonEvmBridgeDestination,
    },
}

impl BridgeLeg<'_> {
    /// The EVM delivery of a leg that shields to the wallet on its destination chain.
    pub(super) const fn private_delivery(&self) -> Option<(BridgeDelivery, &EffectiveChainConfig)> {
        match self {
            Self::Chain {
                delivery, chain, ..
            } if delivery.is_private() => Some((*delivery, *chain)),
            _ => None,
        }
    }
}

/// A Bridge order's provider terms quoted while signing, or the term that changed since the
/// review. Nothing is signed for a change.
pub(super) enum BridgeSigning {
    Terms(BridgeOrderTerms),
    Changed(SwapReviewChange),
}

/// What the signing-time Across quote of a private delivery names besides the preview's terms:
/// the destination chain's handler, the fill's recipient, and the handler message.
#[derive(Clone, Copy)]
pub(super) struct AcrossHandlerMessage<'a> {
    pub(super) handler: Address,
    pub(super) message: &'a Bytes,
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
    /// 1Click's USD estimate of `expected_output`, in micro-dollars, for display only: never a
    /// price anchor or a rate check. `None` for Across.
    pub expected_output_usd: Option<U256>,
    /// The bridge's cost in bought-token base units. `None` for an unverified leg between
    /// different assets.
    pub fee: Option<U256>,
    pub leg: BridgeLegPrice,
    pub fill_time_sec: Option<u64>,
    /// The destination-side terms of a private delivery, whose `destination_minimum` and
    /// `expected_output` are what the destination stealth account receives before its shield.
    pub private: Option<SwapPrivateBridgeQuote>,
}

/// The destination-side terms of a private Bridge delivery's preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapPrivateBridgeQuote {
    /// Across's quoted output before the delivery allowance.
    pub quoted_output: U256,
    /// The shield's gas on the destination chain, in destination-token base units.
    pub delivery_allowance: U256,
    /// The destination chain's shield fee rate, in basis points.
    pub destination_shield_fee_bps: U256,
    /// The least deposit Across is expected to take with the handler message, in input-token
    /// base units, estimated from the preview quote. `None` when it gives no estimate.
    pub deposit_floor: Option<U256>,
}

impl SwapBridgeQuote {
    /// The minimum the receiver ends up with: the destination minimum, and for a private
    /// delivery that minimum less the destination chain's shield fee on it.
    #[must_use]
    pub fn received_minimum(&self) -> U256 {
        self.private.map_or(self.destination_minimum, |private| {
            self.destination_minimum
                - railgun_protocol_fee_amount(
                    self.destination_minimum,
                    private.destination_shield_fee_bps,
                )
        })
    }
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

    /// Quote `leg`'s provider on `clients` for `buy_amount` of the bought token, the order's buy
    /// amount. A private delivery's Across quote is the same request, and its output is then
    /// reduced by the delivery allowance. No anchor prices a non-EVM token, so a non-EVM leg is
    /// verified only by the same-asset rule.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn quote_swap_bridge(
        &self,
        clients: &SwapBridgeClients,
        leg: &BridgeLeg<'_>,
        profile: &SwapProfile,
        buy_amount: U256,
        slippage_bps: u32,
        anchor_cache: Option<&TokenAnchorRateCache>,
        token_registry: &EffectiveTokenRegistry,
    ) -> Result<SwapBridgeQuote> {
        let quote = match leg {
            BridgeLeg::Chain {
                delivery,
                destination,
                chain,
            } => match delivery.provider {
                BridgeProvider::Across => {
                    let spoke_pool = self
                        .chain
                        .bridge_profile()
                        .ok_or_else(|| eyre!("this network doesn't support bridging"))?
                        .spoke_pool();
                    quote_across_preview(
                        &self.http,
                        &clients.across,
                        AcrossOrigin {
                            chain_id: self.chain.chain_id,
                            spoke_pool,
                            token: destination.intermediate,
                        },
                        chain,
                        *delivery,
                        buy_amount,
                        anchor_cache,
                        token_registry,
                    )
                    .await?
                }
                BridgeProvider::NearIntents => {
                    let assets = destination
                        .near
                        .as_ref()
                        .ok_or_else(|| eyre!("NEAR Intents doesn't list this destination"))?;
                    let quote = near_dry_quote(
                        &clients.near,
                        assets,
                        profile,
                        buy_amount,
                        slippage_bps,
                        None,
                    )
                    .await?;
                    let anchor = anchor_cache.and_then(|cache| {
                        cache
                            .cached_bridge_leg_rate(
                                self.chain.chain_id,
                                destination.intermediate,
                                delivery.destination_chain,
                                anchor_token(
                                    delivery.destination_chain,
                                    delivery.destination_token,
                                ),
                                token_registry,
                            )
                            .ok()
                            .flatten()
                    });
                    near_bridge_quote(
                        buy_amount,
                        &quote,
                        anchor,
                        assets,
                        destination.same_asset,
                        profile.anchor_deviation_bps(),
                    )?
                }
            },
            BridgeLeg::NonEvm {
                delivery,
                destination,
            } => {
                let quote = near_dry_quote(
                    &clients.near,
                    &destination.near,
                    profile,
                    buy_amount,
                    slippage_bps,
                    Some(delivery.network),
                )
                .await?;
                near_bridge_quote(
                    buy_amount,
                    &quote,
                    None,
                    &destination.near,
                    destination.same_asset,
                    profile.anchor_deviation_bps(),
                )?
            }
        };
        if quote.destination_minimum.is_zero() {
            return Err(eyre!("nothing is left to receive after the bridge fee"));
        }
        Ok(quote)
    }

    /// Quote `leg`'s provider on `clients` again for `review`'s approved order: `buy_amount` of
    /// the bought token, from an order valid until `valid_to`, for at least
    /// `destination_minimum` on the destination chain. A 1Click quote names the receiver and the
    /// executor, so it isn't requested when a leg verified at review can no longer be priced. A
    /// private delivery's Across quote carries `handler_message`, which it requires, so Across
    /// simulates the real fill. A message that fails there gets no quote, which is an error, not
    /// a lower quote.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn bridge_signing_terms(
        &self,
        review: &SwapReview,
        clients: &SwapBridgeClients,
        leg: &BridgeLeg<'_>,
        buy_amount: U256,
        destination_minimum: U256,
        valid_to: u32,
        handler_message: Option<AcrossHandlerMessage<'_>>,
        profile: &SwapProfile,
        anchor_cache: &TokenAnchorRateCache,
        token_registry: &EffectiveTokenRegistry,
    ) -> Result<BridgeSigning> {
        let plan = review.plan();
        if leg.private_delivery().is_some() != handler_message.is_some() {
            return Err(eyre!(
                "only a private bridge delivery is quoted with a handler message"
            ));
        }
        let signing = NearSigning {
            review,
            near: &clients.near,
            buy_amount,
            destination_minimum,
            valid_to,
            profile,
        };
        match leg {
            BridgeLeg::Chain {
                delivery,
                destination,
                ..
            } => match delivery.provider {
                BridgeProvider::Across => {
                    let spoke_pool = self
                        .chain
                        .bridge_profile()
                        .ok_or_else(|| eyre!("this network doesn't support bridging"))?
                        .spoke_pool();
                    let fee = AcrossFeeRequest {
                        input_token: plan.buy_token(),
                        output_token: delivery.destination_token,
                        origin_chain: self.chain.chain_id,
                        destination_chain: delivery.destination_chain,
                        amount: buy_amount,
                    };
                    let fees = match handler_message {
                        Some(private) => clients
                            .across
                            .suggested_fees_with_message(&AcrossMessageFeeRequest {
                                fee,
                                recipient: private.handler,
                                message: private.message.clone(),
                            })
                            .await
                            .map_err(|error| match error {
                                BridgeApiError::FillSimulationFailed => eyre!(
                                    "Across can't deliver this swap now: the shield on the destination network fails in its simulation. The order wasn't placed."
                                ),
                                BridgeApiError::AmountTooLow => eyre!(
                                    "Across won't bridge this amount: the shield's gas on the destination network is too large a share of it. The order wasn't placed. Try a larger amount."
                                ),
                                error => error.into(),
                            })?,
                        None => clients.across.suggested_fees(&fee).await?,
                    };
                    across_order_terms(
                        &fees,
                        spoke_pool,
                        plan.buy_token(),
                        *delivery,
                        buy_amount,
                        destination_minimum,
                        valid_to,
                        handler_message
                            .map(|private| (private.handler, keccak256(private.message))),
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
                    signing
                        .order_terms(assets, anchor, OneClickRecipient::Evm(delivery.receiver))
                        .await
                }
            },
            // 1Click checks the receiver only now, since review quotes send a placeholder.
            BridgeLeg::NonEvm {
                delivery,
                destination,
            } => signing
                .order_terms(
                    &destination.near,
                    None,
                    OneClickRecipient::NonEvm(delivery.receiver.clone()),
                )
                .await
                .map_err(|error| match error.downcast::<BridgeApiError>() {
                    Ok(error) => non_evm_quote_error(error),
                    Err(error) => error,
                }),
        }
    }
}

/// A 1Click dry quote of `buy_amount` between `assets`, for the profile's window. The
/// placeholder recipient is a `non_evm` network's address when the destination is one.
async fn near_dry_quote(
    near: &NearIntentsClient,
    assets: &NearAssets,
    profile: &SwapProfile,
    buy_amount: U256,
    slippage_bps: u32,
    non_evm: Option<NonEvmNetwork>,
) -> Result<OneClickDryQuote> {
    let deadline = SystemTime::now()
        .checked_add(profile.valid_to_window() + NEAR_QUOTE_DEADLINE_MARGIN)
        .ok_or_else(|| eyre!("the bridge quote's deadline is out of range"))?;
    Ok(near
        .dry_quote(
            &OneClickQuoteParams {
                origin_asset: assets.origin_asset.clone(),
                destination_asset: assets.destination_asset.clone(),
                amount: buy_amount,
                slippage_bps,
                deadline,
            },
            non_evm,
        )
        .await?)
}

/// What a 1Click quote for an approved order needs besides its assets, anchor and recipient.
struct NearSigning<'a> {
    review: &'a SwapReview,
    near: &'a NearIntentsClient,
    buy_amount: U256,
    destination_minimum: U256,
    valid_to: u32,
    profile: &'a SwapProfile,
}

impl NearSigning<'_> {
    /// The 1Click terms of the approved order, which pays `recipient` with refunds to the
    /// executor. A leg verified at review must still have its `anchor`, and the quote must
    /// deliver the approved minimum within the anchor's deviation.
    async fn order_terms(
        &self,
        assets: &NearAssets,
        anchor: Option<PairAnchorRate>,
        recipient: OneClickRecipient,
    ) -> Result<BridgeSigning> {
        let review = self.review;
        if anchor.is_none()
            && review
                .bridge()
                .is_some_and(|bridge| matches!(bridge.leg, BridgeLegPrice::Verified(_)))
        {
            return Ok(BridgeSigning::Changed(SwapReviewChange::PriceUnavailable));
        }
        let deadline = UNIX_EPOCH
            .checked_add(Duration::from_secs(u64::from(self.valid_to)) + NEAR_QUOTE_DEADLINE_MARGIN)
            .ok_or_else(|| eyre!("the bridge quote's deadline is out of range"))?;
        let executor = review.plan().executor();
        // Refunds return to the executor on this chain.
        let quote = self
            .near
            .quote(
                &OneClickQuoteParams {
                    origin_asset: assets.origin_asset.clone(),
                    destination_asset: assets.destination_asset.clone(),
                    amount: self.buy_amount,
                    slippage_bps: review.slippage_bps(),
                    deadline,
                },
                recipient,
                executor,
                executor,
            )
            .await?;
        if quote.min_amount_out < self.destination_minimum {
            return Ok(BridgeSigning::Changed(
                SwapReviewChange::DestinationMinimum {
                    approved: self.destination_minimum,
                    current: quote.min_amount_out,
                },
            ));
        }
        if let Some(rate) = anchor {
            match check_quote_against_anchor(
                self.buy_amount,
                quote.min_amount_out,
                rate,
                self.profile.anchor_deviation_bps(),
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

/// The error of a non-EVM leg's signing quote. 1Click refuses a receiver the wallet's own check
/// accepted with `400 recipient is not valid`, which is reported as such.
pub(super) fn non_evm_quote_error(error: BridgeApiError) -> eyre::Report {
    match error {
        BridgeApiError::Rejected {
            api: BridgeApi::NearIntents,
            status: 400,
            message,
            ..
        } if message.contains("recipient") => eyre!(
            "NEAR Intents rejected the receiver: {message}. The order wasn't placed. Check the receiver address."
        ),
        error => error.into(),
    }
}

/// What an Across preview quote needs of the chain a swap deposits on: its id, its pinned
/// `SpokePool`, and the token deposited there.
#[derive(Clone, Copy)]
pub(super) struct AcrossOrigin {
    pub(super) chain_id: u64,
    pub(super) spoke_pool: Address,
    pub(super) token: Address,
}

/// Across's preview quote for depositing `amount` of `origin`'s token for `delivery`. The
/// request names only tokens, chains and the amount. A private delivery's output is then
/// reduced by its delivery allowance, read on `destination_chain`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn quote_across_preview(
    http: &HttpContext,
    across: &AcrossClient,
    origin: AcrossOrigin,
    destination_chain: &EffectiveChainConfig,
    delivery: BridgeDelivery,
    amount: U256,
    anchor_cache: Option<&TokenAnchorRateCache>,
    token_registry: &EffectiveTokenRegistry,
) -> Result<SwapBridgeQuote> {
    let fees = across
        .suggested_fees(&AcrossFeeRequest {
            input_token: origin.token,
            output_token: delivery.destination_token,
            origin_chain: origin.chain_id,
            destination_chain: delivery.destination_chain,
            amount,
        })
        .await?;
    let allowance = if delivery.is_private() {
        Some(
            private_delivery_allowance(
                http,
                destination_chain,
                delivery,
                &fees,
                anchor_cache,
                token_registry,
            )
            .await?,
        )
    } else {
        None
    };
    across_bridge_quote(&fees, origin.spoke_pool, allowance)
}

/// The delivery allowance of a private `delivery` for the Across quote `fees`, in
/// destination-token base units: [`private_delivery_gas`] on the destination chain's gas
/// model, converted by [`delivery_allowance_rate`]. With a rate, the gas is priced at the
/// destination chain's RPC gas price with the 25% cushion of the order's gas estimate, plus
/// the rollup data cost of the handler message there. Scaled from the quote, it needs no
/// request.
async fn private_delivery_allowance(
    http: &HttpContext,
    destination_chain: &EffectiveChainConfig,
    delivery: BridgeDelivery,
    fees: &AcrossFeeQuote,
    anchor_cache: Option<&TokenAnchorRateCache>,
    token_registry: &EffectiveTokenRegistry,
) -> Result<U256> {
    let chain_id = delivery.destination_chain;
    let wrapped_native = crate::amounts::wrapped_native_token_for_chain(chain_id);
    // Destination-token base units per whole native token, like the `buy_rate` that
    // prices the order's gas estimate on the swap's chain.
    let anchor = anchor_cache
        .zip(wrapped_native)
        .and_then(|(cache, native)| {
            cache
                .cached_pair_rate(chain_id, native, delivery.destination_token, token_registry)
                .ok()
                .flatten()
                .map(|rate| rate.buy_rate)
        });
    let rate = delivery_allowance_rate(delivery, wrapped_native, anchor, fees)?;
    let (gas_price_wei, data_cost_wei) =
        if matches!(rate, DeliveryAllowanceRate::QuoteScaled { .. }) {
            (0, U256::ZERO)
        } else {
            let chain = effective_desktop_chain_config(chain_id, destination_chain)?;
            let fee_candidates = acquire_fee_candidates(
                http,
                &chain.rpc_route.for_class(crate::RpcTrafficClass::General)?,
                None,
            )
            .await?;
            // The data cost counts two hex characters per byte, as in app data.
            let message_len = placeholder_private_delivery_message_len(delivery)?;
            let (gas_price_wei, data_cost_wei) = tokio::try_join!(
                gas_price_from_candidates_with_policy(&fee_candidates, 1, 1),
                hook_data_cost_from_candidates(
                    &fee_candidates,
                    chain_id,
                    message_len.saturating_mul(2),
                    &chain.gas,
                ),
            )?;
            (
                gas_price_wei
                    .checked_add(gas_price_wei.div_ceil(4))
                    .ok_or(OrderLimitError::Overflow)?,
                data_cost_wei,
            )
        };
    Ok(delivery_allowance(&DeliveryAllowanceParams {
        gas: private_delivery_gas(
            RailgunGasModel::for_chain(chain_id),
            GasEstimateMode::UpperBound,
        ),
        gas_price_wei,
        data_cost_wei,
        rate,
    })?)
}

/// The validated route of `plan`'s Bridge delivery, which requires one; other deliveries take
/// none. The route must bridge the plan's bought token to the delivery's token and chain or
/// network with the delivery's provider, and returns its clients with the checked leg.
pub(super) fn bridge_route<'a>(
    plan: &SwapInputPlan,
    route: Option<SwapBridgeRoute<'a>>,
) -> Result<Option<(&'a SwapBridgeClients, BridgeLeg<'a>)>> {
    let mismatch = || eyre!("the bridge route doesn't match the swap's delivery");
    match (plan.delivery(), route) {
        (SwapDelivery::Bridge(delivery), Some(route)) => {
            let SwapBridgeDestination::Chain { destination, chain } = route.destination else {
                return Err(mismatch());
            };
            if destination.intermediate != plan.buy_token()
                || destination.destination_token != delivery.destination_token
                || chain.chain_id != delivery.destination_chain
                || (delivery.provider == BridgeProvider::Across) != destination.near.is_none()
            {
                return Err(mismatch());
            }
            Ok(Some((
                route.clients,
                BridgeLeg::Chain {
                    delivery,
                    destination,
                    chain,
                },
            )))
        }
        (SwapDelivery::NonEvmBridge(delivery), Some(route)) => {
            let SwapBridgeDestination::NonEvm(destination) = route.destination else {
                return Err(mismatch());
            };
            if destination.intermediate != plan.buy_token()
                || destination.near.destination_asset != delivery.asset_id
                || destination.contract != delivery.contract
            {
                return Err(mismatch());
            }
            Ok(Some((
                route.clients,
                BridgeLeg::NonEvm {
                    delivery,
                    destination,
                },
            )))
        }
        (SwapDelivery::Bridge(_) | SwapDelivery::NonEvmBridge(_), None) => {
            Err(eyre!("a bridge swap needs its bridge route"))
        }
        (_, Some(_)) => Err(eyre!("only a bridge swap takes a bridge route")),
        (_, None) => Ok(None),
    }
}

/// How a private `delivery`'s allowance converts the destination chain's gas cost into its
/// token: at par for `wrapped_native`, that chain's wrapped native token, else by `anchor`,
/// the cached rate in destination-token base units per whole native token, else scaled from
/// the relayer gas fee of the quote `fees`.
pub(super) fn delivery_allowance_rate(
    delivery: BridgeDelivery,
    wrapped_native: Option<Address>,
    anchor: Option<U256>,
    fees: &AcrossFeeQuote,
) -> Result<DeliveryAllowanceRate> {
    if wrapped_native == Some(delivery.destination_token) {
        return Ok(DeliveryAllowanceRate::Par);
    }
    if let Some(rate) = anchor {
        return Ok(DeliveryAllowanceRate::Anchor(rate));
    }
    Ok(DeliveryAllowanceRate::QuoteScaled {
        relayer_gas_fee: fees
            .relayer_gas_fee_in_output()
            .ok_or_else(|| eyre!("Across's quote can't price the delivery on that network"))?,
    })
}

/// An Across fee quote for the order's buy amount. Its output is the destination minimum and
/// its relay fee, which includes the LP fee, the bridge fee. The quote must name the swap
/// chain's `SpokePool`: the wallet never takes the deposit contract from the API. A private
/// delivery's preview passes its delivery `allowance`: the destination minimum is then the
/// output less the allowance, the amount the deposit signs for the destination stealth
/// account, and the allowance is reported beside the fee.
pub(super) fn across_bridge_quote(
    fees: &AcrossFeeQuote,
    spoke_pool: Address,
    allowance: Option<U256>,
) -> Result<SwapBridgeQuote> {
    if fees.spoke_pool != spoke_pool {
        return Err(eyre!("Across quoted a deposit into an unknown SpokePool"));
    }
    let (output, private) = match allowance {
        Some(allowance) => (
            destination_minimum_after_allowance(fees.output_amount, allowance)?,
            Some(SwapPrivateBridgeQuote {
                quoted_output: fees.output_amount,
                delivery_allowance: allowance,
                destination_shield_fee_bps: RAILGUN_PROTOCOL_FEE_BPS,
                deposit_floor: private_deposit_floor(fees, allowance),
            }),
        ),
        None => (fees.output_amount, None),
    };
    Ok(SwapBridgeQuote {
        provider: BridgeProvider::Across,
        destination_minimum: output,
        expected_output: output,
        expected_output_usd: None,
        fee: Some(fees.total_relay_fee_total),
        leg: BridgeLegPrice::SameAsset,
        fill_time_sec: Some(fees.estimated_fill_time_sec),
        private,
    })
}

/// The least deposit Across is expected to take once the quote carries a private delivery's
/// handler message, in input-token base units, estimated from the preview quote `fees` alone.
/// Across's least deposit is proportional to the relayer's gas fee, and the message adds the
/// shield's gas, which `allowance` estimates in destination-token base units:
/// `min_deposit * (gas_fee + allowance) / gas_fee`, with the gas fee in output-token units like
/// the allowance. `None` when the quote states no gas fee or no usable least deposit, which
/// Across gives as zero or as the largest integer.
fn private_deposit_floor(fees: &AcrossFeeQuote, allowance: U256) -> Option<U256> {
    if fees.relayer_gas_fee_total.is_zero()
        || fees.min_deposit.is_zero()
        || fees.min_deposit == U256::MAX
    {
        return None;
    }
    let gas_fee = fees
        .relayer_gas_fee_in_output()
        .filter(|gas_fee| !gas_fee.is_zero())?;
    fees.min_deposit
        .checked_mul(gas_fee.checked_add(allowance)?)
        .map(|scaled| scaled / gas_fee)
}

/// The deposit terms of a fresh Across quote for an approved order: `buy_amount` of
/// `input_token` for the approved `destination_minimum`, not the quote's own output. The quote
/// must name the swap chain's `SpokePool`. Its fill deadline must leave relayers 30 minutes
/// after the order expires, and its timestamp and fill deadline must pass the `SpokePool`'s
/// buffers for any settlement before `valid_to`: a deposit's block can't precede its quote. No
/// delivery allowance applies: the approved minimum already allows for it. A private delivery's
/// terms carry `handler_message`, the handler the deposit pays and its message's hash. A public
/// delivery's receiver must not be the destination `SpokePool` the quote names.
#[allow(clippy::too_many_arguments)]
pub(super) fn across_order_terms(
    fees: &AcrossFeeQuote,
    spoke_pool: Address,
    input_token: Address,
    delivery: BridgeDelivery,
    buy_amount: U256,
    destination_minimum: U256,
    valid_to: u32,
    handler_message: Option<(Address, B256)>,
) -> Result<BridgeSigning> {
    let quote = across_bridge_quote(fees, spoke_pool, None)?;
    // The only use of the destination pool the provider names: it can only refuse a swap. A
    // private delivery's receiver is the wallet's own account, and its deposit pays the handler.
    if !delivery.is_private() && delivery.receiver == fees.destination_spoke_pool {
        return Err(BridgeReceiverRejection::SpokePool.into());
    }
    if quote.destination_minimum < destination_minimum {
        return Ok(BridgeSigning::Changed(
            SwapReviewChange::DestinationMinimum {
                approved: destination_minimum,
                current: quote.destination_minimum,
            },
        ));
    }
    if !across_quote_covers(fees.timestamp, fees.fill_deadline, valid_to) {
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
            recipient: handler_message.map(|(handler, _)| handler),
            message_hash: handler_message.map(|(_, hash)| hash),
        },
    )))
}

/// Whether an Across quote taken at `quote_timestamp` with `fill_deadline` can be deposited by
/// any settlement before `valid_to`: the fill deadline leaves relayers 30 minutes after the
/// order expires, and the timestamp and the fill deadline pass the `SpokePool`'s buffers.
pub(super) fn across_quote_covers(quote_timestamp: u32, fill_deadline: u32, valid_to: u32) -> bool {
    let timestamp = u64::from(quote_timestamp);
    let fill_deadline = u64::from(fill_deadline);
    let valid_to = u64::from(valid_to);
    fill_deadline >= valid_to + ACROSS_FILL_MARGIN_SECS
        && fill_deadline <= timestamp + ACROSS_FILL_DEADLINE_BUFFER_SECS
        && timestamp + ACROSS_QUOTE_TIME_BUFFER_SECS >= valid_to
}

/// The `depositV3` call of an Across order's post-hook: `terms`, deposited by the executor for
/// their recipient, `delivery`'s receiver or a private delivery's handler, with an empty
/// message. A private delivery's post-hook adds the handler message.
pub(super) fn across_deposit(
    executor: Address,
    delivery: BridgeDelivery,
    terms: &AcrossOrderTerms,
) -> SpokePool::depositV3Call {
    SpokePool::depositV3Call {
        depositor: executor,
        recipient: terms.deposit_recipient(delivery),
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

/// A 1Click dry quote for `buy_amount` of the intermediate in `assets`. Its minimum output is
/// the destination minimum. With an `anchor` rate for the leg, that minimum must be within
/// `deviation_bps` of it, and the fee is `buy_amount` less the minimum's value in the
/// intermediate. Without one, a `same_asset` leg's fee is `buy_amount` less the minimum in the
/// intermediate's decimals, and a leg between different assets is unverified.
pub(super) fn near_bridge_quote(
    buy_amount: U256,
    quote: &OneClickDryQuote,
    anchor: Option<PairAnchorRate>,
    assets: &NearAssets,
    same_asset: bool,
    deviation_bps: u32,
) -> Result<SwapBridgeQuote> {
    let minimum = quote.min_amount_out;
    let (fee, leg) = match anchor {
        Some(rate) => {
            check_quote_against_anchor(buy_amount, minimum, rate, deviation_bps)?;
            let value =
                U512::from(minimum) * U512::from(rate.sell_rate) / U512::from(rate.buy_rate);
            (
                Some(buy_amount.saturating_sub(U256::saturating_from(value))),
                BridgeLegPrice::Verified(rate),
            )
        }
        None if same_asset => (
            Some(buy_amount.saturating_sub(rescale(
                minimum,
                assets.destination_decimals,
                assets.origin_decimals,
            ))),
            BridgeLegPrice::SameAsset,
        ),
        None => (None, BridgeLegPrice::Unverified),
    };
    Ok(SwapBridgeQuote {
        provider: BridgeProvider::NearIntents,
        destination_minimum: minimum,
        expected_output: quote.amount_out,
        expected_output_usd: quote.amount_out_usd,
        fee,
        leg,
        fill_time_sec: quote.time_estimate_sec,
        private: None,
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
