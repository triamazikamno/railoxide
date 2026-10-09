//! The review of a swap paid from a Public account, and the order path's signed batch and
//! order.
//!
//! Everything runs on the destination chain's owner, with the chain the account pays on passed
//! in. The review names the Public account, which that chain already sees, and nothing of the
//! destination stealth account: its `CoW` quote carries no hooks, and its Across quote no
//! recipient or message.
//!
//! An order is paid to the account's cow-shed proxy. Its one post-hook is the proxy's hook
//! batch, which the account signs first: a balance guard, then a weiroll script that deposits
//! the proxy's whole balance of the bought token into Across. The account then signs the order
//! whose app data carries that batch. Both are in the swap's record before the orderbook
//! request, the only one that carries them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::dyn_abi::{Resolver, TypedData};
use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::providers::Provider as _;
use alloy::sol_types::{Eip712Domain, SolCall as _, SolStruct};
use broadcaster_core::contracts::across::private_delivery_message;
use broadcaster_core::contracts::cow::{
    AppData, AppDataHook, EncodedAppData, Order, OrderUid, eip712_order_signature, order_digest,
    order_uid, settlement_domain,
};
use broadcaster_core::contracts::cow_shed::{
    COWShedFactory, DepositHook, ExecuteHooks, decode_deposit_hook_calls, deposit_hook_calls,
    execute_hooks_calldata, execute_hooks_digest, proxy_address, proxy_domain,
};
use broadcaster_core::contracts::executor::AcrossPrivateDelivery;
use broadcaster_core::contracts::swap_math::SWAP_MATH_RUNTIME_CODE_HASH;
use broadcaster_core::contracts::weiroll::BalanceDeposit;
use broadcaster_core::query_rpc_pool::QueryRpcPool;
use eyre::{Result, eyre};
use railgun_wallet::tx::GasEstimateMode;
use serde_json::{Value, json};

use super::bridge::{
    AcrossOrigin, BridgeLegPrice, SwapBridgeQuote, across_bridge_quote, across_quote_covers,
    quote_across_preview,
};
use super::gas::hook_data_cost_from_rpc_pool;
use super::order::{
    SwapGasPricing, SwapOrderOutcome, SwapPrice, anchor_token, approval_cushion,
    order_limit_or_tight, placeholder_destination_shield_multicall, swap_gas_pricing, swap_order,
    swap_order_limit, swap_submission_outcome, swap_submission_status, valid_to_after,
};
use super::public_source::PublicSwapDeliveryQuote;
use super::public_transactions::{
    AuthorizedPublicSwapSource, ClaimedPublicSwap, PublicSwapGasPlan, proxy_deployed,
    require_public_swap_source,
};
use crate::bridge::{AcrossClient, PublicBridgeDestination, PublicBridgePath, PublicSellAsset};
use crate::cow::{
    CowOrderSubmission, CowOrderbookClient, CowQuote, CowQuoteParameters, CowSellQuoteRequest,
    OrderLimit, OrderLimitError, hook_gas_limit, public_deposit_hook_gas, quote_protocol_fee,
};
use crate::desktop::executor_observation::trace_step;
use crate::desktop::{gas_price_from_rpc_pool_with_policy, query_rpc_pool_with_http_client};
use crate::public_wallet::{VaultedPublicSigner, query_erc20_balance, sign_public_swap_typed_data};
use crate::settings::{EffectiveChainConfig, EffectiveTokenRegistry, PublicSwapProfile};
use crate::vault::{
    AcrossOrderTerms, BridgeDelivery, BridgePrivateDelivery, BridgeProvider, BridgeShieldFailure,
    BridgeSurplus, ExecutorOperationId, PublicSwapApproval, PublicSwapHookBatch, PublicSwapIntent,
    PublicSwapOrder, PublicSwapPath, SwapApprovedAccount, SwapApprovedBounds, SwapSubmission,
    SwapSubmissionStatus, SwapUseId,
};
use crate::{ExecutorOwner, TokenAnchorRateCache, check_quote_against_anchor};

const ORDER_DATA_TOO_LARGE: &str = "the swap's order data is too large for CoW's orderbook";
const NO_PUBLIC_SWAPS: &str = "swaps aren't available from this network";
/// How long before its `validTo` a signed order is no longer resent: at least one request
/// timeout, for the orderbook to accept it.
pub(super) const RESUBMISSION_MARGIN: Duration = Duration::from_mins(1);

/// Why a swap paid from a Public account is refused on the network it pays on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PublicSwapUnavailable {
    /// The network has no order parameters, or the math contract an order's post-hook calls
    /// isn't deployed there: the hook would fail after the trade and strand the proceeds in
    /// the account's proxy.
    #[error(
        "Swaps aren't available from this network yet. Only tokens that bridge directly can be sent."
    )]
    OrdersNotEnabled { chain_id: u64 },
}

/// Whether the code at `math` on the chain `pool` reads is the wallet's math contract, from the
/// first of its endpoints that answers.
async fn swap_math_deployed(pool: &QueryRpcPool, math: Address) -> Result<bool> {
    for endpoint in pool.available_providers() {
        if let Ok(code) = endpoint.provider.get_code_at(math).await {
            return Ok(keccak256(&code) == SWAP_MATH_RUNTIME_CODE_HASH);
        }
    }
    Err(eyre!("the network the swap pays on is unavailable"))
}

/// What a review of a swap paid from a Public account needs. Nothing in it names the
/// destination stealth account.
pub struct PublicSwapReviewRequest<'a> {
    /// The chain the Public account pays on.
    pub origin: &'a EffectiveChainConfig,
    /// The Public account. An order's `CoW` quote names it as the owner.
    pub source: Address,
    pub sell: PublicSellAsset,
    pub sell_amount: U256,
    /// The destination token on this chain, the token bridged to reach it, and the path.
    pub destination: &'a PublicBridgeDestination,
    /// The price tolerance on an order's best case, in basis points.
    pub slippage_bps: u32,
    /// The share of an order's gas estimate its minimum deducts, in basis points of 10,000.
    pub gas_share_bps: u16,
    pub on_shield_failure: BridgeShieldFailure,
    /// The orderbook of the chain the account pays on. Required exactly for an order.
    pub orderbook: Option<&'a CowOrderbookClient>,
    pub across: &'a AcrossClient,
    pub anchor_cache: Option<&'a TokenAnchorRateCache>,
    pub token_registry: &'a EffectiveTokenRegistry,
    /// The fee the Public account pays for its own transactions.
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// Terms the user reviews before approving a swap paid from a Public account.
#[derive(Debug, Clone)]
pub struct PublicSwapReview {
    path: PublicBridgePath,
    /// `Address::ZERO` for the native asset.
    sell_token: Address,
    bridged_token: Address,
    sell_amount: U256,
    slippage_bps: u32,
    on_shield_failure: BridgeShieldFailure,
    /// The chain the Public account pays on and its pinned `SpokePool`, which the bridge leg
    /// is previewed from.
    origin_chain: u64,
    spoke_pool: Address,
    /// The previewed delivery. Its receiver is a placeholder.
    delivery: BridgeDelivery,
    bridge: SwapBridgeQuote,
    gas_plan: PublicSwapGasPlan,
    price: SwapPrice,
    /// `None` for a direct deposit.
    order: Option<PublicOrderReview>,
}

/// The order of a reviewed swap.
#[derive(Debug, Clone)]
pub(super) struct PublicOrderReview {
    /// The hook-free quote.
    quote: CowQuoteParameters,
    /// `CoW`'s protocol fee in bought-token units, when the quote states it.
    cow_fee: Option<U256>,
    limit: OrderLimit,
    /// The quote-time gas inputs, kept so that another gas share reprices without I/O.
    gas: SwapGasPricing,
    /// The conservative gas estimate of the order's post-hook, which the limit prices.
    hook_gas: u64,
    /// The share the limit was priced at, the Tight preset when the requested one left no
    /// positive minimum.
    gas_share_bps: u16,
    proxy: Address,
    hook_gas_limit: u64,
    valid_for_secs: u32,
    quote_id: Option<i64>,
}

impl PublicSwapReview {
    #[must_use]
    pub const fn path(&self) -> PublicBridgePath {
        self.path
    }
    #[must_use]
    pub const fn sell_amount(&self) -> U256 {
        self.sell_amount
    }
    /// The order's buy amount; `None` for a direct deposit.
    #[must_use]
    pub fn buy_amount(&self) -> Option<U256> {
        self.order.as_ref().map(|order| order.limit.buy_amount)
    }
    /// The bridge leg, quoted for the sold amount of a direct deposit or an order's buy
    /// amount.
    #[must_use]
    pub const fn bridge(&self) -> &SwapBridgeQuote {
        &self.bridge
    }
    /// Replace this review's preview with the actual delivery costs that stopped signing.
    /// The route and input must still be the ones reviewed. This changes no saved approval;
    /// the resulting terms need explicit approval before signing can resume.
    fn with_delivery_quote(&self, quote: &PublicSwapDeliveryQuote) -> Result<Self> {
        let request = quote.request;
        if request.origin_chain != self.origin_chain
            || request.destination_chain != self.delivery.destination_chain
            || request.input_token != self.bridged_token
            || request.output_token != self.delivery.destination_token
            || request.amount != self.buy_amount().unwrap_or(self.sell_amount)
        {
            return Err(eyre!("the delivery quote differs from the reviewed swap"));
        }
        let original = self
            .bridge
            .private
            .ok_or_else(|| eyre!("the reviewed swap has no private delivery"))?;
        // The real message already priced the shield's gas; adding the preview allowance
        // again would charge it twice. Across's real minimum deposit needs no extrapolation.
        let mut bridge = across_bridge_quote(&quote.fees, self.spoke_pool, Some(U256::ZERO))?;
        if let Some(private) = &mut bridge.private {
            private.destination_shield_fee_bps = original.destination_shield_fee_bps;
            private.deposit_floor = (!quote.fees.min_deposit.is_zero()
                && quote.fees.min_deposit != U256::MAX)
                .then_some(quote.fees.min_deposit);
        }
        bridge.leg = self.bridge.leg;
        Ok(Self {
            bridge,
            ..self.clone()
        })
    }

    /// An order's hook-free `CoW` quote, for display only; `None` for a direct deposit.
    #[must_use]
    pub fn quote(&self) -> Option<&CowQuoteParameters> {
        self.order.as_ref().map(|order| &order.quote)
    }
    /// `CoW`'s protocol fee in bought-token base units on the chain the account pays on,
    /// already deducted from the quote. `None` for a direct deposit, and when the quote
    /// doesn't state it.
    #[must_use]
    pub fn cow_fee(&self) -> Option<U256> {
        self.order.as_ref().and_then(|order| order.cow_fee)
    }
    /// What the private balance receives if solvers pay all of an order's gas, in
    /// destination-token base units: the order limit's best case at this review's ratio of
    /// the minimum received on the destination chain, after the bridge and delivery costs and
    /// the shield fee there, to the order's buy amount. The hook deposits the proxy's whole
    /// balance and scales the output by that ratio. `None` for a direct deposit, whose amount
    /// is exact.
    #[must_use]
    pub fn best_case(&self) -> Option<U256> {
        let limit = self.order.as_ref()?.limit;
        limit
            .best_case
            .saturating_mul(self.bridge.received_minimum())
            .checked_div(limit.min_received)
    }
    /// The share of an order's gas estimate its minimum deducts, in basis points of 10,000:
    /// the Tight preset when the requested share left no positive minimum.
    #[must_use]
    pub fn gas_share_bps(&self) -> Option<u16> {
        self.order.as_ref().map(|order| order.gas_share_bps)
    }
    /// The price tolerance on an order's best case, in basis points.
    #[must_use]
    pub const fn slippage_bps(&self) -> u32 {
        self.slippage_bps
    }
    /// The least amount of the bridged token Across is expected to take for this delivery, in
    /// its base units on the chain the account pays on, when the reviewed deposit is below it:
    /// the sold amount of a direct deposit, an order's buy amount. Such a review can't be
    /// approved. `None` when the deposit reaches it, and when the quote gave no estimate.
    #[must_use]
    pub fn too_small_to_bridge(&self) -> Option<U256> {
        let deposit = self.buy_amount().unwrap_or(self.sell_amount);
        self.bridge
            .private
            .and_then(|private| private.deposit_floor)
            .filter(|floor| deposit < *floor)
    }
    /// The limit of this review's order at another gas share, priced from the reviewed quote
    /// and gas inputs without I/O. Its amounts are in the bought token on the chain the
    /// account pays on. The bridge leg was quoted for the reviewed buy amount:
    /// [`ExecutorOwner::requote_public_swap_gas_share`] returns the review at that share.
    pub fn order_limit_at(&self, gas_share_bps: u16) -> Result<OrderLimit> {
        Ok(self.order_at_gas_share(gas_share_bps)?.limit)
    }

    /// This review's order priced at another gas share, without I/O.
    fn order_at_gas_share(&self, gas_share_bps: u16) -> Result<PublicOrderReview> {
        let order = self
            .order
            .as_ref()
            .ok_or_else(|| eyre!("a direct deposit has no order to price"))?;
        let limit = swap_order_limit(
            order.hook_gas,
            self.bridged_token,
            &order.quote,
            order.gas,
            self.slippage_bps,
            gas_share_bps,
            U256::ZERO,
        )?;
        Ok(PublicOrderReview {
            limit,
            gas_share_bps,
            ..order.clone()
        })
    }

    /// A review of `order`, or of a direct deposit, whose bridge leg is `bridge`.
    #[cfg(test)]
    // The quote is moved into the review.
    #[allow(clippy::large_types_passed_by_value)]
    pub(super) const fn for_tests(
        bridged_token: Address,
        sell_amount: U256,
        slippage_bps: u32,
        bridge: SwapBridgeQuote,
        order: Option<PublicOrderReview>,
    ) -> Self {
        let on_shield_failure = BridgeShieldFailure::RefundOnOrigin;
        Self {
            path: if order.is_some() {
                PublicBridgePath::Order
            } else {
                PublicBridgePath::Deposit
            },
            sell_token: Address::ZERO,
            bridged_token,
            sell_amount,
            slippage_bps,
            on_shield_failure,
            origin_chain: 1,
            spoke_pool: Address::ZERO,
            delivery: BridgeDelivery {
                provider: BridgeProvider::Across,
                destination_chain: 137,
                receiver: Address::ZERO,
                destination_token: bridged_token,
                surplus: BridgeSurplus::Reshield,
                private: Some(BridgePrivateDelivery { on_shield_failure }),
            },
            bridge,
            gas_plan: PublicSwapGasPlan {
                approval_gas_limits: Vec::new(),
                deposit_gas_limit: None,
                max_fee_per_gas: 0,
                max_priority_fee_per_gas: 0,
                max_gas_cost: U256::ZERO,
            },
            price: SwapPrice::Unverified,
            order,
        }
    }
    /// The Public account's own transactions: its approvals, and the deposit on the direct
    /// path.
    #[must_use]
    pub const fn gas_plan(&self) -> &PublicSwapGasPlan {
        &self.gas_plan
    }
    /// The cow-shed proxy of the Public account, for an order.
    #[must_use]
    pub fn proxy(&self) -> Option<Address> {
        self.order.as_ref().map(|order| order.proxy)
    }
    /// The post-hook's gas limit, for an order.
    #[must_use]
    pub fn hook_gas_limit(&self) -> Option<u64> {
        self.order.as_ref().map(|order| order.hook_gas_limit)
    }
    /// The `CoW` quote's id, for an order whose quote has one. The order submission names it.
    #[must_use]
    pub fn quote_id(&self) -> Option<i64> {
        self.order.as_ref().and_then(|order| order.quote_id)
    }
    /// An order's `CoW` quote checked against the pair's cached anchors on the chain the
    /// account pays on. A direct deposit has no such quote: its price is the bridge leg's
    /// cached cross-chain rate, when there is one, and isn't compared with anything.
    /// [`Self::price_verified`] says whether approval needs an acknowledgement.
    #[must_use]
    pub const fn price(&self) -> &SwapPrice {
        &self.price
    }
    /// Whether the swap's price was checked: an order's `CoW` price and the bridge leg, and
    /// for a direct deposit the bridge leg only. Otherwise approval requires the user's
    /// acknowledgement.
    #[must_use]
    pub fn price_verified(&self) -> bool {
        self.bridge.leg != BridgeLegPrice::Unverified
            && (self.order.is_none() || self.price != SwapPrice::Unverified)
    }
    /// What the swap deposits and by which path.
    #[must_use]
    pub const fn intent(&self) -> PublicSwapIntent {
        PublicSwapIntent {
            bridged_token: self.bridged_token,
            order: self.order.is_some(),
        }
    }

    /// The approval this review binds, for the destination account `destination` and the setup
    /// fee approved for it.
    pub fn approval(
        &self,
        destination: SwapApprovedAccount,
        destination_setup_fee: Option<U256>,
        price_acknowledged: bool,
    ) -> Result<PublicSwapApproval> {
        if self.bridge.destination_minimum.is_zero() {
            return Err(eyre!(
                "approve a minimum received on the destination network for this swap"
            ));
        }
        if !self.price_verified() && !price_acknowledged {
            return Err(eyre!(
                "acknowledge the unverified price before approving this swap"
            ));
        }
        if self.too_small_to_bridge().is_some() {
            return Err(eyre!(
                "this amount is too small to bridge to the private balance on the destination network"
            ));
        }
        let order = self.order.as_ref();
        let private = self.bridge.private;
        Ok(PublicSwapApproval {
            bounds: SwapApprovedBounds {
                sell_amount: self.sell_amount,
                unshield_amount: None,
                unshield_fee_bps: U256::ZERO,
                // A direct deposit bridges what it sells.
                buy_amount: order.map_or(self.sell_amount, |order| order.limit.buy_amount),
                private_minimum: self.bridge.received_minimum(),
                shield_fee_bps: U256::ZERO,
                slippage_bps: self.slippage_bps,
                pre_hook_gas_limit: 0,
                post_hook_gas_limit: order.map(|order| order.hook_gas_limit),
                hook_cost: order.map(|order| order.limit.gas_estimate),
                anchors: match &self.price {
                    SwapPrice::Verified { observations, .. } => observations.clone(),
                    SwapPrice::Unverified => Vec::new(),
                },
                destination_minimum: Some(self.bridge.destination_minimum),
                gas_share_bps: order.map(|order| order.gas_share_bps),
                gas_estimate: order.map(|order| order.limit.gas_estimate),
                gas_allowance: order.map(|order| order.limit.gas_allowance),
                gas_price_wei: order.map(|order| order.gas.gas_price_wei),
                valid_for_secs: order.map(|order| order.valid_for_secs),
                destination_shield_fee_bps: private
                    .map(|private| private.destination_shield_fee_bps),
                delivery_allowance: private.map(|private| private.delivery_allowance),
                destination_setup_fee,
                source_setup_fee: None,
            },
            price_verified: Some(self.price_verified()),
            price_acknowledged,
            sell_token: self.sell_token,
            on_shield_failure: self.on_shield_failure,
            destination,
            max_gas_cost: self.gas_plan.max_gas_cost,
        })
    }
}

/// The review of an order that buys `bridged_token` for its proxy, priced from the hook-free
/// `quote` as a private swap's review is: the quote's swap gas and `hook_gas` at the RPC gas
/// price with its cushion, plus the hook's data cost. The order carries no shield, so its buy
/// amount is its minimum. A `gas_share_bps` that leaves no positive minimum is priced at the
/// Tight preset instead.
#[allow(clippy::too_many_arguments)]
pub(super) fn price_public_order(
    quote: CowQuote,
    price: &SwapPrice,
    gas_price_wei: u128,
    hook_data_cost_wei: U256,
    hook_gas: u64,
    bridged_token: Address,
    slippage_bps: u32,
    gas_share_bps: u16,
    proxy: Address,
    hook_gas_limit: u64,
    valid_for_secs: u32,
) -> Result<PublicOrderReview, OrderLimitError> {
    let gas = swap_gas_pricing(&quote.quote, price, gas_price_wei, hook_data_cost_wei)?;
    let (limit, gas_share_bps) = order_limit_or_tight(
        hook_gas,
        bridged_token,
        &quote.quote,
        gas,
        slippage_bps,
        gas_share_bps,
        U256::ZERO,
    )?;
    Ok(PublicOrderReview {
        cow_fee: quote_protocol_fee(&quote),
        quote: quote.quote,
        limit,
        gas,
        hook_gas,
        gas_share_bps,
        proxy,
        hook_gas_limit,
        valid_for_secs,
        quote_id: quote.id,
    })
}

/// The amount the signed `terms` of a swap approved with `bounds` deposit: an order's buy
/// amount, or what a direct deposit sells. An order's is the approved buy amount, raised by at
/// most the approval's cushion when the bridge quote fell short while signing. A direct
/// deposit's is the approved amount exactly. Terms for any other amount are refused.
pub(super) fn signed_input_amount(
    bounds: &SwapApprovedBounds,
    intent: PublicSwapIntent,
    terms: &AcrossOrderTerms,
) -> Result<U256> {
    let cushion = if intent.order {
        approval_cushion(bounds.gas_allowance.unwrap_or_default())
    } else {
        U256::ZERO
    };
    let amount = terms.input_amount;
    if amount < bounds.buy_amount || amount - bounds.buy_amount > cushion {
        return Err(eyre!(
            "the order's terms differ from the approved swap; review the swap again"
        ));
    }
    Ok(amount)
}

/// The hook batch of an order: the balance guard and the deposit script for `terms` and
/// `delivery`, run by `source`'s cow-shed proxy under `nonce` until `valid_to`. Returned with
/// the script it runs and the proxy.
pub(crate) fn public_order_hooks(
    profile: &PublicSwapProfile,
    source: Address,
    spoke_pool: Address,
    buy_amount: U256,
    destination_minimum: U256,
    destination_chain: u64,
    terms: &AcrossOrderTerms,
    delivery: &AcrossPrivateDelivery,
    nonce: B256,
    valid_to: u32,
) -> Result<(ExecuteHooks, BalanceDeposit, Address)> {
    let proxy = proxy_address(
        profile.cow_shed_factory(),
        profile.cow_shed_implementation(),
        source,
    );
    let deposit = BalanceDeposit {
        proxy,
        math: profile.math(),
        spoke_pool,
        buy_amount,
        destination_min: destination_minimum,
        depositor: source,
        input_token: terms.input_token,
        output_token: terms.output_token,
        destination_chain_id: destination_chain,
        exclusive_relayer: terms.exclusive_relayer,
        quote_timestamp: terms.quote_timestamp,
        fill_deadline: terms.fill_deadline,
        exclusivity_parameter: terms.exclusivity_parameter,
        delivery: delivery.clone(),
    };
    let hooks = ExecuteHooks {
        calls: deposit_hook_calls(profile.weiroll(), &deposit)?,
        nonce,
        deadline: U256::from(valid_to),
    };
    Ok((hooks, deposit, proxy))
}

/// The fill-or-kill sell order of a swap paid from a Public account and its app data: it pays
/// `proxy`, and its one post-hook is `hook_calldata`, the signed `executeHooks` call on the
/// cow-shed factory. An app data document beyond the profile's byte budget is an error.
pub(crate) fn public_order(
    profile: &PublicSwapProfile,
    sell_token: Address,
    buy_token: Address,
    sell_amount: U256,
    buy_amount: U256,
    valid_to: u32,
    proxy: Address,
    hook_calldata: Bytes,
    hook_gas_limit: u64,
) -> Result<(Order, EncodedAppData)> {
    let app_data = public_order_app_data(profile, hook_calldata, hook_gas_limit)?;
    let order = swap_order(
        sell_token,
        buy_token,
        proxy,
        sell_amount,
        buy_amount,
        valid_to,
        app_data.hash,
    );
    Ok((order, app_data))
}

/// App data with `hook_calldata` on the cow-shed factory as its only hook, a post-hook.
fn public_order_app_data(
    profile: &PublicSwapProfile,
    hook_calldata: Bytes,
    hook_gas_limit: u64,
) -> Result<EncodedAppData> {
    let app_data = AppData::hooks(
        profile.app_code().to_owned(),
        Vec::new(),
        vec![AppDataHook {
            call_data: hook_calldata,
            gas_limit: hook_gas_limit,
            target: profile.cow_shed_factory(),
        }],
    )
    .encode()?;
    if app_data.document.len() > profile.app_data_byte_budget() {
        return Err(eyre!(ORDER_DATA_TOO_LARGE));
    }
    Ok(app_data)
}

/// The length of the app data of an order whose delivery shields `destination_token` with
/// `shield_multicall`, or an error when it is beyond the profile's byte budget. Every other
/// argument of the hook is a static ABI word and the signature is 65 bytes, so placeholders
/// give the signed hook's length. Nothing here names an account or a signed payload.
pub(crate) fn public_order_app_data_len(
    profile: &PublicSwapProfile,
    destination_token: Address,
    shield_multicall: Bytes,
    hook_gas_limit: u64,
) -> Result<usize> {
    let deposit = BalanceDeposit {
        proxy: Address::ZERO,
        math: profile.math(),
        spoke_pool: Address::ZERO,
        buy_amount: U256::ONE,
        destination_min: U256::ONE,
        depositor: Address::ZERO,
        input_token: Address::ZERO,
        output_token: destination_token,
        destination_chain_id: 0,
        exclusive_relayer: Address::ZERO,
        quote_timestamp: u32::MAX,
        fill_deadline: u32::MAX,
        exclusivity_parameter: u32::MAX,
        delivery: AcrossPrivateDelivery {
            handler: Address::ZERO,
            destination_executor: Address::ZERO,
            shield_multicall,
            fallback: None,
        },
    };
    let hook = COWShedFactory::executeHooksCall {
        calls: deposit_hook_calls(profile.weiroll(), &deposit)?,
        nonce: B256::ZERO,
        deadline: U256::from(u32::MAX),
        user: Address::ZERO,
        signature: Bytes::from_static(&[0; 65]),
    }
    .abi_encode();
    Ok(public_order_app_data(profile, hook.into(), hook_gas_limit)?
        .document
        .len())
}

/// A fresh nonce for an order's hook batch, from the system's random source. The caller draws
/// it before the order is signed, so the batch's terms can be shown first.
pub fn new_public_swap_batch_nonce() -> Result<B256> {
    let mut nonce = B256::ZERO;
    getrandom::fill(&mut nonce.0)
        .map_err(|_| eyre!("the system's random source is unavailable"))?;
    Ok(nonce)
}

/// What a hook batch does, for a review beside a hardware device's prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicSwapBatchTerms {
    /// The cow-shed proxy that runs the batch.
    pub proxy: Address,
    /// The batch reverts unless the proxy holds `guard_amount` of `guard_token`.
    pub guard_token: Address,
    pub guard_amount: U256,
    /// Whom Across refunds: the Public account.
    pub depositor: Address,
    /// The destination chain's handler, which the deposit pays.
    pub recipient: Address,
    pub input_token: Address,
    pub output_token: Address,
    pub destination_chain: u64,
    /// The deposit's output is the proxy's balance times `scale_numerator` over
    /// `scale_denominator`.
    pub scale_numerator: U256,
    pub scale_denominator: U256,
    /// Unix seconds after which the batch can't run.
    pub deadline: u32,
    pub nonce: B256,
}

/// What a hardware review shows before the device prompt: the decoded terms of the batch
/// `hooks`, whose script is `deposit`.
#[must_use]
pub fn public_swap_batch_terms(
    hooks: &ExecuteHooks,
    deposit: &BalanceDeposit,
) -> PublicSwapBatchTerms {
    PublicSwapBatchTerms {
        proxy: deposit.proxy,
        guard_token: deposit.input_token,
        guard_amount: deposit.buy_amount,
        depositor: deposit.depositor,
        recipient: deposit.delivery.handler,
        input_token: deposit.input_token,
        output_token: deposit.output_token,
        destination_chain: deposit.destination_chain_id,
        scale_numerator: deposit.destination_min,
        scale_denominator: deposit.buy_amount,
        deadline: hooks.deadline.saturating_to(),
        nonce: hooks.nonce,
    }
}

/// The EIP-712 payload of `message`, a value of the struct `S`, in `domain`. The pinned
/// `TypedData::from_struct` needs `S: Serialize`, which the shared contract structs don't
/// derive, so the types come from `S` and the caller supplies the message's JSON.
fn typed_data<S: SolStruct>(domain: Eip712Domain, message: Value) -> Result<Value> {
    let mut resolver = Resolver::from_struct::<S>();
    resolver.ingest_string(domain.encode_type())?;
    Ok(serde_json::to_value(TypedData {
        domain,
        resolver,
        primary_type: S::NAME.to_owned(),
        message,
    })?)
}

/// The EIP-712 payload a Public account signs for the hook batch `hooks` of its `proxy`.
pub(crate) fn execute_hooks_typed_data(
    hooks: &ExecuteHooks,
    chain_id: u64,
    proxy: Address,
) -> Result<Value> {
    let calls: Vec<Value> = hooks
        .calls
        .iter()
        .map(|call| {
            json!({
                "target": call.target,
                "value": call.value,
                "callData": call.callData,
                "allowFailure": call.allowFailure,
                "isDelegateCall": call.isDelegateCall,
            })
        })
        .collect();
    typed_data::<ExecuteHooks>(
        proxy_domain(chain_id, proxy),
        json!({
            "calls": calls,
            "nonce": hooks.nonce,
            "deadline": hooks.deadline,
        }),
    )
}

/// The EIP-712 payload a Public account signs for `order` at `settlement`.
pub(crate) fn order_typed_data(order: &Order, chain_id: u64, settlement: Address) -> Result<Value> {
    typed_data::<Order>(
        settlement_domain(chain_id, settlement),
        json!({
            "sellToken": order.sellToken,
            "buyToken": order.buyToken,
            "receiver": order.receiver,
            "sellAmount": order.sellAmount,
            "buyAmount": order.buyAmount,
            "validTo": order.validTo,
            "appData": order.appData,
            "feeAmount": order.feeAmount,
            "kind": order.kind,
            "partiallyFillable": order.partiallyFillable,
            "sellTokenBalance": order.sellTokenBalance,
            "buyTokenBalance": order.buyTokenBalance,
        }),
    )
}

/// Approval of a reviewed order from a Public account, for a signed delivery.
pub struct PublicSwapOrderRequest<'a> {
    pub operation: ExecutorOperationId,
    pub swap_use: SwapUseId,
    /// The chain the Public account pays on.
    pub origin: &'a EffectiveChainConfig,
    pub source: &'a AuthorizedPublicSwapSource,
    /// The orderbook of the chain the account pays on.
    pub orderbook: &'a CowOrderbookClient,
    /// The delivery and terms `sign_public_swap_delivery` returned, quoted for `valid_to`.
    /// Their input amount is the order's buy amount.
    pub delivery: &'a AcrossPrivateDelivery,
    pub terms: &'a AcrossOrderTerms,
    /// The order's `validTo`, Unix seconds, also the batch's deadline.
    pub valid_to: u32,
    /// The nonce of the order's hook batch, from [`new_public_swap_batch_nonce`]: the one the
    /// batch's terms were shown with. A resume of an order that is already recorded keeps the
    /// recorded batch and ignores it.
    pub batch_nonce: B256,
    pub quote_id: Option<i64>,
    /// The user confirmed signing a hash on a hardware device that can't show typed data, as
    /// the `WalletConnect` flow asks.
    pub hash_fallback_confirmed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicSwapOrderOutcome {
    /// The batch and the order were persisted, then the orderbook accepted the order.
    Submitted { uid: OrderUid },
    /// The proxy holds the bought token or its balance couldn't be read: nothing was signed.
    ProxyHoldsBoughtToken {
        proxy: Address,
        /// `None` when the balance couldn't be read.
        balance: Option<U256>,
    },
}

impl ExecutorOwner {
    /// Whether the Public accounts of `origin` can place orders: the chain has a
    /// [`PublicSwapProfile`] and the math contract an order's post-hook calls is deployed
    /// there with the pinned runtime code. A chain that can't be read is an error.
    pub async fn public_swap_orders_available(
        &self,
        origin: &EffectiveChainConfig,
    ) -> Result<bool> {
        let Some(profile) = origin.public_swap_profile() else {
            return Ok(false);
        };
        let pool = query_rpc_pool_with_http_client(origin.rpc_route.endpoint_urls(), &self.http);
        self.while_active(swap_math_deployed(&pool, profile.math()))
            .await
    }

    /// Refuse an order on `origin` unless [`Self::public_swap_orders_available`] holds.
    async fn require_public_swap_orders(&self, origin: &EffectiveChainConfig) -> Result<()> {
        if self.public_swap_orders_available(origin).await? {
            Ok(())
        } else {
            Err(PublicSwapUnavailable::OrdersNotEnabled {
                chain_id: origin.chain_id,
            }
            .into())
        }
    }

    /// Quote a swap paid from the Public account `request.source` on another chain and
    /// delivered to this chain's private balance, and plan the account's own transactions. A
    /// direct deposit takes Across's preview quote for the sold amount. An order first takes
    /// a hook-free `CoW` quote, checks it against cached anchors, prices the order limit, and
    /// takes Across's quote for the order's buy amount.
    pub async fn review_public_swap(
        &self,
        request: PublicSwapReviewRequest<'_>,
    ) -> Result<PublicSwapReview> {
        self.while_active(Box::pin(self.review_public_swap_active(request)))
            .await
    }

    async fn review_public_swap_active(
        &self,
        request: PublicSwapReviewRequest<'_>,
    ) -> Result<PublicSwapReview> {
        let PublicSwapReviewRequest {
            origin,
            source,
            sell,
            sell_amount,
            destination,
            slippage_bps,
            gas_share_bps,
            on_shield_failure,
            orderbook,
            across,
            anchor_cache,
            token_registry,
            max_fee_per_gas,
            max_priority_fee_per_gas,
        } = request;
        if origin.chain_id == self.chain.chain_id {
            return Err(eyre!(
                "a swap paid from a Public account delivers to another network than it pays on"
            ));
        }
        let spoke_pool = origin
            .bridge_origin_profile()
            .ok_or_else(|| eyre!("this network has no pinned Across SpokePool"))?
            .spoke_pool();
        self.chain
            .bridge_profile()
            .ok_or_else(|| eyre!("private Bridge delivery is unavailable on this chain"))?;
        let path = destination.path;
        let destination = &destination.destination;
        if destination.near.is_some() {
            return Err(eyre!(
                "only Across delivers a swap paid from a Public account"
            ));
        }
        let bridged_token = destination.intermediate;
        let destination_token = destination.destination_token;
        // The preview names no account: the receiver is a placeholder, and only its ABI width
        // enters the delivery allowance.
        let delivery = BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: self.chain.chain_id,
            receiver: Address::ZERO,
            destination_token,
            surplus: BridgeSurplus::Reshield,
            private: Some(BridgePrivateDelivery { on_shield_failure }),
        };
        let (sell_token, deposited) = match sell {
            PublicSellAsset::Erc20(token) => (token, token),
            PublicSellAsset::Native { wrapped } => (Address::ZERO, wrapped),
        };

        let (order, price, spender) = match path {
            PublicBridgePath::Deposit => {
                if deposited != bridged_token {
                    return Err(eyre!(
                        "a direct deposit bridges the token it sells; this destination takes an order"
                    ));
                }
                // No `CoW` quote prices a deposit. The bridge leg's cached rate is its price.
                let price = anchor_cache
                    .and_then(|cache| {
                        cache
                            .cached_bridge_leg_rate(
                                origin.chain_id,
                                bridged_token,
                                self.chain.chain_id,
                                anchor_token(self.chain.chain_id, destination_token),
                                token_registry,
                            )
                            .ok()
                            .flatten()
                    })
                    .map_or(SwapPrice::Unverified, |rate| SwapPrice::Verified {
                        rate,
                        observations: Vec::new(),
                    });
                (None, price, spoke_pool)
            }
            PublicBridgePath::Order => {
                let profile = origin
                    .public_swap_profile()
                    .ok_or_else(|| eyre!(NO_PUBLIC_SWAPS))?;
                if sell_token == Address::ZERO {
                    return Err(eyre!("an order can't sell the network's native asset"));
                }
                if sell_token == bridged_token {
                    return Err(eyre!(
                        "an order buys another token than it sells; this destination takes a direct deposit"
                    ));
                }
                let orderbook = orderbook
                    .ok_or_else(|| eyre!("an order's review needs its network's orderbook"))?;
                // Without the math contract the post-hook fails after the trade.
                self.require_public_swap_orders(origin).await?;
                let valid_for = profile.valid_to_window();
                let valid_for_secs = u32::try_from(valid_for.as_secs())
                    .map_err(|_| eyre!("the order's validity is out of range"))?;
                let proxy = proxy_address(
                    profile.cow_shed_factory(),
                    profile.cow_shed_implementation(),
                    source,
                );
                let pool =
                    query_rpc_pool_with_http_client(origin.rpc_route.endpoint_urls(), &self.http);
                // The first batch of an account deploys its proxy, which costs the hook more.
                let deployed = proxy_deployed(&pool, proxy).await?;
                let hook_gas = public_deposit_hook_gas(deployed, GasEstimateMode::UpperBound);
                let hook_gas_limit = hook_gas_limit(hook_gas);
                // An order whose app data can't fit is refused here, before anything is
                // approved or signed.
                let app_data_len = public_order_app_data_len(
                    &profile,
                    destination_token,
                    placeholder_destination_shield_multicall(delivery)?,
                    hook_gas_limit,
                )?;
                let quote = async {
                    orderbook
                        .quote_sell(&CowSellQuoteRequest {
                            sell_token,
                            buy_token: bridged_token,
                            from: source,
                            receiver: proxy,
                            sell_amount_before_fee: sell_amount,
                            valid_to: valid_to_after(SystemTime::now(), valid_for)?,
                        })
                        .await
                        .map_err(eyre::Report::from)
                };
                let (quote, gas_price_wei, hook_data_cost_wei) = tokio::try_join!(
                    quote,
                    gas_price_from_rpc_pool_with_policy(&pool, 1, 1),
                    hook_data_cost_from_rpc_pool(&pool, origin.chain_id, app_data_len, &origin.gas),
                )?;
                let anchor = anchor_cache.map_or(Ok(None), |cache| {
                    cache.cached_pair_rate(
                        origin.chain_id,
                        sell_token,
                        anchor_token(origin.chain_id, bridged_token),
                        token_registry,
                    )
                });
                let price = match anchor {
                    Ok(Some(rate)) => {
                        check_quote_against_anchor(
                            quote.quote.sell_amount,
                            quote.quote.buy_amount,
                            rate,
                            profile.anchor_deviation_bps(),
                        )?;
                        SwapPrice::Verified {
                            rate,
                            observations: Vec::new(),
                        }
                    }
                    Ok(None) | Err(_) => SwapPrice::Unverified,
                };
                let order = price_public_order(
                    quote,
                    &price,
                    gas_price_wei,
                    hook_data_cost_wei,
                    hook_gas,
                    bridged_token,
                    slippage_bps,
                    gas_share_bps,
                    proxy,
                    hook_gas_limit,
                    valid_for_secs,
                )?;
                (Some(order), price, profile.vault_relayer())
            }
        };

        let bridged_amount = order
            .as_ref()
            .map_or(sell_amount, |order| order.limit.buy_amount);
        let bridge = quote_across_preview(
            &self.http,
            across,
            AcrossOrigin {
                chain_id: origin.chain_id,
                spoke_pool,
                token: bridged_token,
            },
            &self.chain,
            delivery,
            bridged_amount,
            anchor_cache,
            token_registry,
        )
        .await?;
        let gas_plan = self
            .plan_public_swap_gas(
                origin,
                source,
                sell_token,
                spender,
                sell_amount,
                order.is_none(),
                max_fee_per_gas,
                max_priority_fee_per_gas,
            )
            .await?;
        Ok(PublicSwapReview {
            path,
            sell_token,
            bridged_token,
            sell_amount,
            slippage_bps,
            on_shield_failure,
            origin_chain: origin.chain_id,
            spoke_pool,
            delivery,
            bridge,
            gas_plan,
            price,
            order,
        })
    }

    /// Review the actual delivery costs that stopped signing, with only the Public account's
    /// remaining transactions priced at its originally reviewed fee rates. Approval already
    /// paid on-chain must not be charged again. No bridge preview or order quote is requested.
    pub async fn requote_public_swap_delivery(
        &self,
        review: &PublicSwapReview,
        quote: &PublicSwapDeliveryQuote,
        origin: &EffectiveChainConfig,
        source: Address,
    ) -> Result<PublicSwapReview> {
        self.while_active(Box::pin(async {
            if origin.chain_id != review.origin_chain
                || review.delivery.destination_chain != self.chain.chain_id
            {
                return Err(eyre!("the reviewed swap belongs to another network"));
            }
            let mut corrected = review.with_delivery_quote(quote)?;
            let spender = if review.order.is_some() {
                origin
                    .public_swap_profile()
                    .ok_or_else(|| eyre!(NO_PUBLIC_SWAPS))?
                    .vault_relayer()
            } else {
                review.spoke_pool
            };
            corrected.gas_plan = self
                .plan_public_swap_gas(
                    origin,
                    source,
                    review.sell_token,
                    spender,
                    review.sell_amount,
                    review.order.is_none(),
                    review.gas_plan.max_fee_per_gas,
                    review.gas_plan.max_priority_fee_per_gas,
                )
                .await?;
            Ok(corrected)
        }))
        .await
    }

    /// `review`, of an order, at another gas share, with its bridge leg previewed again for
    /// the new order buy amount. The `CoW` quote, the gas inputs, the validity and the
    /// account's own gas plan stay as they were reviewed, and the orderbook isn't asked. The
    /// preview still names no recipient and carries no message.
    pub async fn requote_public_swap_gas_share(
        &self,
        review: &PublicSwapReview,
        gas_share_bps: u16,
        across: &AcrossClient,
        anchor_cache: Option<&TokenAnchorRateCache>,
        token_registry: &EffectiveTokenRegistry,
    ) -> Result<PublicSwapReview> {
        self.while_active(Box::pin(async {
            if review.delivery.destination_chain != self.chain.chain_id {
                return Err(eyre!("this swap delivers to another network"));
            }
            let order = review.order_at_gas_share(gas_share_bps)?;
            let bridge = quote_across_preview(
                &self.http,
                across,
                AcrossOrigin {
                    chain_id: review.origin_chain,
                    spoke_pool: review.spoke_pool,
                    token: review.bridged_token,
                },
                &self.chain,
                review.delivery,
                order.limit.buy_amount,
                anchor_cache,
                token_registry,
            )
            .await?;
            Ok(PublicSwapReview {
                bridge,
                order: Some(order),
                ..review.clone()
            })
        }))
        .await
    }

    /// Sign, persist and submit the order of a swap paid from a Public account, for a signed
    /// delivery. The account signs its proxy's hook batch, then the order.
    pub async fn submit_public_swap_order(
        &self,
        request: PublicSwapOrderRequest<'_>,
    ) -> Result<PublicSwapOrderOutcome> {
        let PublicSwapOrderRequest {
            operation,
            swap_use,
            origin,
            source,
            orderbook,
            delivery,
            terms,
            valid_to,
            batch_nonce,
            quote_id,
            hash_fallback_confirmed,
        } = request;
        Box::pin(self.submit_public_swap_order_with_signer(
            operation,
            swap_use,
            origin,
            source.signer(),
            orderbook,
            delivery,
            terms,
            valid_to,
            batch_nonce,
            quote_id,
            hash_fallback_confirmed,
        ))
        .await
    }

    /// The decoded terms of the hook batch an order for `delivery`, `terms` and `valid_to`
    /// signs under `batch_nonce`, for a review before the Public account's device prompt. The
    /// hooks are built as the submission builds them, from the same record and arguments.
    pub fn public_swap_order_batch_terms(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        delivery: &AcrossPrivateDelivery,
        terms: &AcrossOrderTerms,
        valid_to: u32,
        batch_nonce: B256,
    ) -> Result<PublicSwapBatchTerms> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        let profile = origin
            .public_swap_profile()
            .ok_or_else(|| eyre!(NO_PUBLIC_SWAPS))?;
        let (hooks, deposit, _) = self.public_swap_order_hooks(
            &claimed,
            &profile,
            delivery,
            terms,
            valid_to,
            batch_nonce,
        )?;
        Ok(public_swap_batch_terms(&hooks, &deposit))
    }

    /// The hook batch of the claimed swap's order, with its script and the proxy: the one
    /// place its inputs are taken from the record, for the terms shown and the batch signed.
    /// Its buy amount is the one `terms` sign, within the approval's cushion.
    fn public_swap_order_hooks(
        &self,
        claimed: &ClaimedPublicSwap,
        profile: &PublicSwapProfile,
        delivery: &AcrossPrivateDelivery,
        terms: &AcrossOrderTerms,
        valid_to: u32,
        batch_nonce: B256,
    ) -> Result<(ExecuteHooks, BalanceDeposit, Address)> {
        let bounds = &claimed.swap.approval().bounds;
        let destination_minimum = bounds
            .destination_minimum
            .ok_or_else(|| eyre!("the swap's approval has no destination minimum"))?;
        public_order_hooks(
            profile,
            claimed.source,
            claimed.spoke_pool,
            signed_input_amount(bounds, claimed.swap.intent(), terms)?,
            destination_minimum,
            self.chain.chain_id,
            terms,
            delivery,
            batch_nonce,
            valid_to,
        )
    }

    /// [`Self::submit_public_swap_order`] with the Public account's signer.
    ///
    /// Nothing is signed while the proxy holds the bought token, or unless the batch decodes
    /// back to the approved terms. The batch and the order are in the record before the
    /// orderbook request. The same request for an order that is already recorded resends it
    /// without a new signature, whatever `batch_nonce` it carries.
    pub(crate) async fn submit_public_swap_order_with_signer(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        signer: &VaultedPublicSigner,
        orderbook: &CowOrderbookClient,
        delivery: &AcrossPrivateDelivery,
        terms: &AcrossOrderTerms,
        valid_to: u32,
        batch_nonce: B256,
        quote_id: Option<i64>,
        hash_fallback_confirmed: bool,
    ) -> Result<PublicSwapOrderOutcome> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        require_public_swap_source(signer, &claimed)?;
        let (approval, intent) = (claimed.swap.approval(), claimed.swap.intent());
        if !intent.order {
            return Err(eyre!(
                "this swap deposits directly; its Public account places no order"
            ));
        }
        let profile = origin
            .public_swap_profile()
            .ok_or_else(|| eyre!(NO_PUBLIC_SWAPS))?;
        match claimed.swap.path() {
            None => {}
            // The same request again resumes the recorded order.
            Some(PublicSwapPath::Order(order))
                if order.valid_to() == valid_to && claimed.swap.bridge() == Some(terms) =>
            {
                let uid = self
                    .resubmit_public_swap_order(operation, swap_use, origin, orderbook)
                    .await?;
                return Ok(PublicSwapOrderOutcome::Submitted { uid });
            }
            Some(_) => {
                return Err(eyre!(
                    "this swap's order is already signed; it can't take other terms"
                ));
            }
        }
        // The review this order follows can be old, so the math contract is read again before
        // anything is signed.
        self.require_public_swap_orders(origin).await?;

        let source = claimed.source;
        let sell_token = approval.sell_token;
        if sell_token == Address::ZERO {
            return Err(eyre!("an order can't sell the network's native asset"));
        }
        let buy_amount = signed_input_amount(&approval.bounds, intent, terms)?;
        let destination_minimum = approval
            .bounds
            .destination_minimum
            .ok_or_else(|| eyre!("the swap's approval has no destination minimum"))?;
        let hook_gas_limit = approval
            .bounds
            .post_hook_gas_limit
            .ok_or_else(|| eyre!("the swap's approval has no hook gas limit"))?;
        let destination = claimed
            .destination
            .filter(|destination| *destination == delivery.destination_executor)
            .ok_or_else(|| {
                eyre!("the destination stealth account changed; review the swap again")
            })?;
        let handler = self
            .chain
            .bridge_profile()
            .ok_or_else(|| eyre!("private Bridge delivery is unavailable on this chain"))?
            .multicall_handler();
        // Without a fallback a failing shield reverts the fill, and Across refunds the deposit.
        let fallback = (approval.on_shield_failure == BridgeShieldFailure::KeepOnDestination)
            .then_some(destination);
        let message = private_delivery_message(
            handler,
            claimed.destination_token,
            destination,
            delivery.shield_multicall.clone(),
            fallback,
        );
        if terms.spoke_pool != claimed.spoke_pool
            || terms.recipient != Some(handler)
            || delivery.handler != handler
            || delivery.fallback != fallback
            || terms.input_token != intent.bridged_token
            || terms.output_token != claimed.destination_token
            || terms.output_amount < destination_minimum
            || terms.message_hash != Some(keccak256(&message))
        {
            return Err(eyre!(
                "the order's terms differ from the approved swap; review the swap again"
            ));
        }
        let now = SystemTime::now();
        let expired = now
            .duration_since(UNIX_EPOCH)
            .is_ok_and(|now| now.as_secs() >= u64::from(valid_to));
        if expired
            || valid_to > valid_to_after(now, profile.valid_to_window())?
            || !across_quote_covers(terms.quote_timestamp, terms.fill_deadline, valid_to)
        {
            return Err(eyre!(
                "the order's validity differs from the one its delivery was quoted for; review the swap again"
            ));
        }

        // A balance the proxy already holds would be deposited with this order's proceeds,
        // or belongs to another swap. Nothing is signed while it is there or can't be read.
        let proxy = proxy_address(
            profile.cow_shed_factory(),
            profile.cow_shed_implementation(),
            source,
        );
        let balance = self
            .while_active(async {
                Ok(
                    query_erc20_balance(&origin.rpc_route, &self.http, intent.bridged_token, proxy)
                        .await,
                )
            })
            .await?;
        match balance {
            Ok(balance) if balance.is_zero() => {}
            balance => {
                return Ok(PublicSwapOrderOutcome::ProxyHoldsBoughtToken {
                    proxy,
                    balance: balance.ok(),
                });
            }
        }

        let (hooks, _, _) = self.public_swap_order_hooks(
            &claimed,
            &profile,
            delivery,
            terms,
            valid_to,
            batch_nonce,
        )?;
        // The batch about to be signed is decoded and compared with a hook derived again from
        // the record's approval and accounts, not from the batch's own inputs. Its buy amount
        // is the signed terms', checked against the approval above.
        let approved = DepositHook {
            weiroll: profile.weiroll(),
            deposit: BalanceDeposit {
                proxy,
                math: profile.math(),
                spoke_pool: claimed.spoke_pool,
                buy_amount,
                destination_min: destination_minimum,
                depositor: claimed.source,
                input_token: intent.bridged_token,
                output_token: claimed.destination_token,
                destination_chain_id: self.chain.chain_id,
                exclusive_relayer: terms.exclusive_relayer,
                quote_timestamp: terms.quote_timestamp,
                fill_deadline: terms.fill_deadline,
                exclusivity_parameter: terms.exclusivity_parameter,
                delivery: AcrossPrivateDelivery {
                    handler,
                    destination_executor: destination,
                    shield_multicall: delivery.shield_multicall.clone(),
                    fallback,
                },
            },
        };
        if decode_deposit_hook_calls(&hooks.calls)? != approved {
            return Err(eyre!(
                "the order's hook batch differs from the approved swap; nothing was signed"
            ));
        }

        let chain_id = origin.chain_id;
        let batch_signature = self
            .while_active(sign_public_swap_typed_data(
                signer,
                execute_hooks_typed_data(&hooks, chain_id, proxy)?,
                execute_hooks_digest(&hooks, chain_id, proxy),
                hash_fallback_confirmed,
            ))
            .await?;
        let hook_calldata = execute_hooks_calldata(
            hooks,
            source,
            &batch_signature,
            chain_id,
            profile.cow_shed_factory(),
            profile.cow_shed_implementation(),
        )?;
        let (order, app_data) = public_order(
            &profile,
            sell_token,
            intent.bridged_token,
            approval.bounds.sell_amount,
            buy_amount,
            valid_to,
            proxy,
            hook_calldata.clone(),
            hook_gas_limit,
        )?;
        let settlement = profile.settlement();
        let digest = order_digest(&order, chain_id, settlement);
        let signature = eip712_order_signature(
            &self
                .while_active(sign_public_swap_typed_data(
                    signer,
                    order_typed_data(&order, chain_id, settlement)?,
                    digest,
                    hash_fallback_confirmed,
                ))
                .await?,
        );
        let uid = OrderUid::new(digest, source, valid_to);

        // The orderbook request carries the signed batch, so it follows the durable write.
        self.ensure_active()?;
        self.store.record_public_swap_path(
            operation,
            swap_use,
            PublicSwapPath::Order(Box::new(PublicSwapOrder::new(
                uid,
                terms.input_token,
                proxy,
                PublicSwapHookBatch::new(hook_calldata, batch_nonce, valid_to),
                SwapSubmission::new(signature, quote_id),
            ))),
            *terms,
        )?;
        self.notify_change();
        let uid = self
            .send_public_swap_order(
                operation,
                swap_use,
                orderbook,
                CowOrderSubmission {
                    order: &order,
                    owner: source,
                    signature: &signature,
                    app_data: &app_data,
                    quote_id,
                },
                uid,
            )
            .await?;
        Ok(PublicSwapOrderOutcome::Submitted { uid })
    }

    /// Resend the persisted signed order without a new signature, after a restart or a lost
    /// response. The order is rebuilt from the record alone: the approval's sold token and
    /// amount and its hook gas limit, and the recorded buy token, signed buy amount, proxy,
    /// `validTo`, signed batch and order signature. An order that doesn't rebuild to its recorded UID isn't sent.
    pub async fn resubmit_public_swap_order(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        origin: &EffectiveChainConfig,
        orderbook: &CowOrderbookClient,
    ) -> Result<OrderUid> {
        let claimed = self.claimed_public_swap(operation, swap_use, origin)?;
        if !claimed.live {
            return Err(eyre!("this swap was stopped"));
        }
        let saved = claimed
            .swap
            .order()
            .ok_or_else(|| eyre!("this swap has no signed order"))?;
        match saved.submission_status() {
            SwapSubmissionStatus::Accepted => return Ok(saved.uid()),
            SwapSubmissionStatus::Rejected => {
                return Err(eyre!("the orderbook rejected this swap's order"));
            }
            SwapSubmissionStatus::Pending => {}
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| eyre!("the system clock is before the Unix epoch"))?
            .as_secs();
        if !claimed.swap.order_can_fill(now) {
            return Err(eyre!("this order no longer awaits submission"));
        }
        // Leave at least one request timeout for the existing order to be accepted.
        if saved.valid_to() <= valid_to_after(SystemTime::now(), RESUBMISSION_MARGIN)? {
            return Err(eyre!(
                "the order is too close to expiry; wait for it to expire"
            ));
        }
        let profile = origin
            .public_swap_profile()
            .ok_or_else(|| eyre!(NO_PUBLIC_SWAPS))?;
        let approval = claimed.swap.approval();
        let hook_gas_limit = approval
            .bounds
            .post_hook_gas_limit
            .ok_or_else(|| eyre!("the swap's approval has no hook gas limit"))?;
        // The signed terms are recorded with the order, and hold the buy amount it was signed
        // with.
        let terms = claimed
            .swap
            .bridge()
            .ok_or_else(|| eyre!("this swap has no signed order"))?;
        let (order, app_data) = public_order(
            &profile,
            approval.sell_token,
            saved.buy_token(),
            approval.bounds.sell_amount,
            signed_input_amount(&approval.bounds, claimed.swap.intent(), terms)?,
            saved.valid_to(),
            saved.proxy(),
            saved.batch().calldata().clone(),
            hook_gas_limit,
        )?;
        let uid = order_uid(
            &order,
            origin.chain_id,
            profile.settlement(),
            claimed.source,
        );
        if uid != saved.uid() {
            return Err(eyre!(
                "the saved order can't be reconstructed unchanged; wait for it to expire"
            ));
        }
        let submission = saved.submission();
        self.send_public_swap_order(
            operation,
            swap_use,
            orderbook,
            CowOrderSubmission {
                order: &order,
                owner: claimed.source,
                signature: submission.signature(),
                app_data: &app_data,
                quote_id: submission.quote_id(),
            },
            uid,
        )
        .await
    }

    /// Send the recorded order `uid` to the orderbook and record what it answered. An
    /// interrupted request leaves the submission pending.
    async fn send_public_swap_order(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        orderbook: &CowOrderbookClient,
        submission: CowOrderSubmission<'_>,
        uid: OrderUid,
    ) -> Result<OrderUid> {
        let submitted = self
            .while_active(async {
                Ok(trace_step(
                    "public_swap_order_submit",
                    orderbook.submit_order(&submission),
                )
                .await)
            })
            .await?;
        self.ensure_active()?;
        self.store.record_public_swap_submission(
            operation,
            swap_use,
            swap_submission_status(&submitted, uid),
        )?;
        self.notify_change();
        let app_data_len = submission.app_data.document.len();
        match swap_submission_outcome(submitted, uid, app_data_len, app_data_len)? {
            SwapOrderOutcome::Submitted { uid } => Ok(uid),
            // The orderbook's size limit is below the profile's budget. No smaller order of
            // the same swap exists, so the rejection ends this order.
            SwapOrderOutcome::Replan { .. } | SwapOrderOutcome::ReviewRequired(_) => {
                Err(eyre!(ORDER_DATA_TOO_LARGE))
            }
        }
    }
}
