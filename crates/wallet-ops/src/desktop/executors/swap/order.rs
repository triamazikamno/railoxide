//! A private swap's order: input planning, review, and the signed hooks and order.
//!
//! Planning selects POI-spendable notes and checks the batch limit and the app-data budget
//! without proving. The review quotes without hooks and prices the order limit. Both run before
//! the setup is confirmed, for a preview or a reserved executor; signing needs the confirmed
//! delegation. Signing checks cached anchors and signs the pre-hook at the executor's current
//! nonce `k`, for Reshield and Across delivery the post-hook at `k + 1`, and the order, persists
//! all of them with the input reservation, and only then sends the order. That request is the
//! only one that carries signed hooks. An External order pays its receiver directly and a NEAR
//! Intents order its verified deposit address; neither has a post-hook.
//!
//! The pre-hook unshields the planned amount, the private spend. Railgun takes its unshield
//! fee from that value, so the order, its quote, and the pre-hook's approval use what the
//! executor then holds, the order's sell amount.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::aliases::U120;
use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::signers::SignerSync as _;
use alloy::sol_types::{SolCall as _, SolValue as _};
use broadcaster_core::contracts::across::SpokePool;
use broadcaster_core::contracts::cow::{
    AppData, AppDataHook, BUY_NATIVE_TOKEN, EncodedAppData, GPv2Settlement, ORDER_KIND_SELL, Order,
    OrderUid, TOKEN_BALANCE_ERC20, eip712_order_signature, order_digest,
};
use broadcaster_core::contracts::executor::{
    ExecutorAction, bridge_deposit_calls, guarded_shield_calls, post_hook_signing_hash,
    signed_post_hook_calldata,
};
use broadcaster_core::contracts::railgun::{
    Call, CommitmentPreimage, RelayAdapt7702, RelayAdapt7702ActionData, ShieldCiphertext,
    ShieldRequest, TokenData, Transaction,
};
use broadcaster_core::contracts::shield::{build_shield_request, derive_shield_private_key};
use eyre::{Result, eyre};
use local_db::PendingOutputPoiContextRecord;
use railgun_wallet::tx::{
    BuildError, CompositeExecution, CompositeUnshieldLeg, CompositeUnshieldLegRole,
    CompositeUnshieldRecipient, ExecutorContext, GasEstimateMode, MAX_BATCH_TRANSACTIONS,
    MAX_CIRCUIT_INPUTS, MixedPrivateActionRebuildConstraint, MixedPrivateActionRequest,
    MixedPrivateOutputRole, RailgunGasModel, SelectedInputIdentity, SwapAmountCheck,
    SwapAppDataTemplate, SwapPostHookTemplate, SwapPreHookSize, TransactionShape,
};
use railgun_wallet::{ProverService, TransactionBuilder, TransactionCall, Utxo};
use reqwest::Url;
use tracing::Instrument as _;
use zeroize::Zeroizing;

use super::bridge::{
    BridgeLegPrice, BridgeSigning, SwapBridgeQuote, SwapBridgeRoute, across_deposit, bridge_route,
};
use super::gas::hook_data_cost_from_rpc_pool;
use super::simulation::{PreHookSimulation, simulate_pre_hook};
use super::{DelegatedSwapExecutor, SwapExecutor, SwapExecutorSetup, trace_step};
use crate::cow::{
    CowApiError, CowOrderSubmission, CowOrderbookClient, CowQuote, CowQuoteParameters,
    CowSellQuoteRequest, NativeBuyRate, OrderLimit, OrderLimitError, OrderLimitParams,
    PreHookCalls, across_post_hook_gas, hook_gas_limit, order_buy_amount, post_hook_gas,
    pre_hook_gas, price_order_limit,
};
use crate::desktop::{
    artifact_source, effective_desktop_chain_config, gas_price_from_rpc_pool_with_policy,
    query_rpc_pool_with_http_client,
};
use crate::poi_contexts::{
    active_list_pre_transaction_pois, build_pending_mixed_output_poi_context_records,
    create_pending_output_poi_contexts,
};
use crate::settings::{EffectiveTokenRegistry, ExecutorProfile, SwapProfile, SwapTokenEligibility};
use crate::vault::{
    BridgeDelivery, BridgeOrderTerms, BridgeProvider, BridgeSurplus, ExecutorInputIdentity,
    ExecutorNonceObservation, ExecutorOperationId, ExecutorPayloadContext, ExecutorPayloadPurpose,
    ExecutorPayloadStatus, ExecutorRecord, ExecutorStoreError, IssuedExecutorPayload,
    SwapAnchorObservation, SwapApproval, SwapApprovalTokens, SwapApprovedBounds, SwapAttempt,
    SwapDelivery, SwapOrderRecord, SwapPreHookDeathCause, SwapProof, SwapRecipient, SwapSubmission,
    SwapSubmissionStatus, SwapTerms,
};
use crate::{
    DesktopPrivateSpendAuthorization, ExecutorOwner, FEE_BASIS_POINTS_DENOMINATOR,
    HardwareExecutorAction, OperationNetworkIsolation, PairAnchorRate, QuoteDeviationError,
    RAILGUN_PROTOCOL_FEE_BPS, TokenAnchorRateCache, WalletSession, check_quote_against_anchor,
    railgun_protocol_fee_amount,
};

/// Widest pre-hook transaction: a full circuit with an unshield and a change output. Eight of
/// them bound the pre-hook gas limit's decimal width in the app-data size estimate.
const WIDEST_PRE_HOOK_TRANSACTION: TransactionShape = TransactionShape {
    input_count: MAX_CIRCUIT_INPUTS,
    output_count: 2,
    has_unshield: true,
};

const SWAP_SPEND_OPERATION: &str = "private swap pre-hook";

/// An amount to plan for a swap, before or after its executor's setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapAmountRequest {
    pub sell_token: Address,
    /// `Address::ZERO` for the native asset, which only External delivery can buy.
    pub buy_token: Address,
    pub amount: U256,
    /// Decides the order's receiver and whether it carries a post-hook.
    pub delivery: SwapDelivery,
    /// The profile's app-data budget when `None`. After a size rejection, the budget from
    /// [`SwapOrderOutcome::Replan`]. It never exceeds the profile's budget.
    pub byte_budget: Option<usize>,
}

/// Notes and transaction shapes for a sell amount that fits the batch limit and the app-data
/// budget, found without proving. The pre-hook proof is built only from such a plan, pinned to
/// its notes, so no proof exists for an amount that doesn't fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapInputPlan {
    executor: SwapExecutor,
    context: ExecutorContext,
    sell_token: Address,
    buy_token: Address,
    delivery: SwapDelivery,
    invalidates: Option<OrderUid>,
    byte_budget: usize,
    size: SwapPreHookSize,
    gas_model: &'static RailgunGasModel,
    /// Estimated post-hook gas, `None` for an order without a post-hook.
    post_hook_gas: Option<u64>,
}

impl SwapInputPlan {
    /// `None` for a preview before the swap's executor is reserved.
    #[must_use]
    pub const fn operation(&self) -> Option<ExecutorOperationId> {
        self.executor.operation()
    }
    #[must_use]
    pub const fn executor(&self) -> Address {
        self.executor.executor()
    }
    #[must_use]
    pub const fn swap_executor(&self) -> SwapExecutor {
        self.executor
    }
    #[must_use]
    pub const fn sell_token(&self) -> Address {
        self.sell_token
    }
    #[must_use]
    pub const fn buy_token(&self) -> Address {
        self.buy_token
    }
    #[must_use]
    pub const fn delivery(&self) -> SwapDelivery {
        self.delivery
    }
    /// The private spend the pre-hook unshields to the executor.
    #[must_use]
    pub const fn amount(&self) -> U256 {
        self.size.amount
    }
    /// Estimated app-data length, at least the length of the document that will be signed.
    #[must_use]
    pub const fn app_data_len(&self) -> usize {
        self.size.app_data_len
    }
    #[must_use]
    pub const fn byte_budget(&self) -> usize {
        self.byte_budget
    }
    #[must_use]
    pub const fn transaction_count(&self) -> usize {
        self.size.preview.shape.transaction_count
    }
    #[must_use]
    pub const fn input_count(&self) -> usize {
        self.size.preview.shape.input_count
    }
    /// An earlier order of this executor that the pre-hook invalidates.
    #[must_use]
    pub const fn invalidates(&self) -> Option<OrderUid> {
        self.invalidates
    }
    /// Gas limit the order declares for the pre-hook.
    #[must_use]
    pub fn pre_hook_gas_limit(&self) -> u64 {
        hook_gas_limit(self.pre_hook_gas(GasEstimateMode::UpperBound))
    }
    /// Gas limit the order declares for the post-hook. `None` for External and NEAR Intents
    /// delivery, whose orders have no post-hook.
    #[must_use]
    pub fn post_hook_gas_limit(&self) -> Option<u64> {
        self.post_hook_estimate().map(hook_gas_limit)
    }
    /// Estimated gas of the order's hooks, which the order limit prices: the pre-hook, and any
    /// post-hook. The declared limits add a margin that only caps execution.
    #[must_use]
    pub fn hook_gas_estimate(&self) -> u64 {
        self.pre_hook_gas(GasEstimateMode::UpperBound)
            .saturating_add(self.post_hook_estimate().unwrap_or(0))
    }

    /// Rebuild the pre-hook from the reviewed notes and shape before proving it.
    pub(crate) fn proof_request(&self, profile: &SwapProfile) -> Result<MixedPrivateActionRequest> {
        // The executor signs the calls after proving; they only fix the plan's shape here.
        let mut request = pre_hook_request(
            self.context,
            self.sell_token,
            self.amount(),
            pre_hook_calls(
                profile,
                self.executor(),
                self.sell_token,
                self.amount(),
                u32::MAX,
                self.invalidates,
            )?,
        );
        request.rebuild = Some(MixedPrivateActionRebuildConstraint {
            selected_inputs: self.size.preview.selected_inputs.clone(),
            expected_shape: self.size.preview.shape,
        });
        Ok(request)
    }

    fn expected_hook_gas(&self) -> u64 {
        self.pre_hook_gas(GasEstimateMode::Expected).saturating_add(
            swap_post_hook_gas(self.gas_model, self.delivery, GasEstimateMode::Expected)
                .unwrap_or(0),
        )
    }

    fn pre_hook_gas(&self, mode: GasEstimateMode) -> u64 {
        pre_hook_gas(
            self.gas_model,
            &self.size.preview.transactions,
            PreHookCalls {
                invalidate_order: self.invalidates.is_some(),
            },
            mode,
        )
    }

    const fn post_hook_estimate(&self) -> Option<u64> {
        self.post_hook_gas
    }
}

/// Estimated gas of `delivery`'s post-hook, `None` for an order without one.
const fn swap_post_hook_gas(
    gas_model: &RailgunGasModel,
    delivery: SwapDelivery,
    mode: GasEstimateMode,
) -> Option<u64> {
    match delivery {
        SwapDelivery::Reshield => Some(post_hook_gas(gas_model, mode)),
        SwapDelivery::External { .. }
        | SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::NearIntents,
            ..
        }) => None,
        SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            surplus,
            ..
        }) => Some(across_post_hook_gas(
            gas_model,
            matches!(surplus, BridgeSurplus::Reshield),
            mode,
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapAmountPlan {
    Fits(SwapInputPlan),
    /// The entered amount needs more transactions than a batch allows, or more app data than
    /// the budget. `largest` is the largest amount below it that fits, to offer instead.
    TooLarge {
        largest: SwapInputPlan,
    },
}

/// How the swap's price was checked at review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapPrice {
    /// The quote is within the profile's deviation from anchor rates. Quote and signing checks
    /// use the background cache without inventing block observations.
    Verified {
        rate: PairAnchorRate,
        observations: Vec<SwapAnchorObservation>,
    },
    /// The pair has no usable independent price. Approval requires the user's acknowledgement.
    Unverified,
}

/// Terms the user reviews before approving a private minimum. The review screen shows them
/// with the fixed approval disclosures: the order, its hooks, and its input nullifiers become
/// public once submitted; a later spend of unfilled inputs is linkable to the swap; the setup
/// cost is paid even without a fill; hook gas is charged to the sell amount; and a pre-hook run
/// without a fill costs both the unshield and the shield fee in recovery.
#[derive(Debug, Clone)]
pub struct SwapReview {
    plan: SwapInputPlan,
    quote: CowQuoteParameters,
    quote_id: Option<i64>,
    limit: OrderLimit,
    estimated_hook_cost: U256,
    surplus_shield_fee_bps: U256,
    shield_fee_bps: U256,
    unshield_fee_bps: U256,
    sell_amount: U256,
    slippage_bps: u32,
    price: SwapPrice,
    isolation: OperationNetworkIsolation,
    /// The bridge leg, for Bridge delivery.
    bridge: Option<SwapBridgeQuote>,
}

impl SwapReview {
    #[must_use]
    pub const fn plan(&self) -> &SwapInputPlan {
        &self.plan
    }
    /// The hook-free quote, for display only.
    #[must_use]
    pub const fn quote(&self) -> &CowQuoteParameters {
        &self.quote
    }
    /// Estimated hook gas cost in buy-token base units, taken off the quoted output.
    #[must_use]
    pub const fn hook_cost(&self) -> U256 {
        self.limit.hook_cost
    }
    /// Expected hook cost for display. Signing still uses the conservative `hook_cost`.
    #[must_use]
    pub const fn estimated_hook_cost(&self) -> U256 {
        self.estimated_hook_cost
    }

    /// Expected `CoW` payout after its quoted fee and the expected hook cost, before delivery.
    #[must_use]
    pub const fn estimated_buy_amount(&self) -> U256 {
        self.quote
            .buy_amount
            .saturating_sub(self.estimated_hook_cost)
    }

    /// Shield fee for a `CoW` payout. Across shields only what remains after its fixed deposit.
    #[must_use]
    pub fn shield_fee_on_output(&self, amount: U256) -> U256 {
        if matches!(
            self.plan.delivery,
            SwapDelivery::Bridge(BridgeDelivery {
                provider: BridgeProvider::Across,
                surplus: BridgeSurplus::Reshield,
                ..
            })
        ) {
            railgun_protocol_fee_amount(
                amount.saturating_sub(self.limit.buy_amount),
                self.surplus_shield_fee_bps,
            )
        } else {
            railgun_protocol_fee_amount(amount, self.shield_fee_bps)
        }
    }

    /// Estimated source-chain return after hook costs and any surplus shield fee.
    /// Only Across deposits a fixed amount and leaves surplus on the source chain.
    #[must_use]
    pub fn estimated_source_surplus(&self) -> Option<U256> {
        matches!(
            self.plan.delivery,
            SwapDelivery::Bridge(BridgeDelivery {
                provider: BridgeProvider::Across,
                ..
            })
        )
        .then(|| {
            let bought = self.estimated_buy_amount();
            bought
                .saturating_sub(self.limit.buy_amount)
                .saturating_sub(self.shield_fee_on_output(bought))
        })
    }
    /// Suggested minimum received privately after the shield fee, or for External delivery,
    /// the minimum the receiver gets.
    #[must_use]
    pub const fn suggested_private_minimum(&self) -> U256 {
        self.limit.min_received
    }
    /// Zero for External delivery: its order carries no shield, so no shield fee applies.
    #[must_use]
    pub const fn shield_fee_bps(&self) -> U256 {
        self.shield_fee_bps
    }
    #[must_use]
    pub const fn unshield_fee_bps(&self) -> U256 {
        self.unshield_fee_bps
    }
    /// The order's `sellAmount`, which the quote priced: the plan's private spend less the
    /// unshield fee, exactly what the executor holds after the pre-hook.
    #[must_use]
    pub const fn sell_amount(&self) -> U256 {
        self.sell_amount
    }
    #[must_use]
    pub const fn slippage_bps(&self) -> u32 {
        self.slippage_bps
    }
    #[must_use]
    pub const fn price(&self) -> &SwapPrice {
        &self.price
    }
    /// The bridge quote for the order's buy amount, for Bridge delivery.
    #[must_use]
    pub const fn bridge(&self) -> Option<&SwapBridgeQuote> {
        self.bridge.as_ref()
    }
    /// Whether both the `CoW` price and any bridge leg were checked. Otherwise approval requires
    /// the user's acknowledgement.
    #[must_use]
    pub fn price_verified(&self) -> bool {
        self.price != SwapPrice::Unverified
            && self
                .bridge
                .is_none_or(|bridge| bridge.leg != BridgeLegPrice::Unverified)
    }
    /// Attach a bridge quote to a review priced without one.
    #[cfg(test)]
    pub(crate) const fn set_bridge_for_tests(&mut self, bridge: SwapBridgeQuote) {
        self.bridge = Some(bridge);
    }
    /// In proxy and direct modes the review states that per-swap isolation is unavailable.
    #[must_use]
    pub const fn isolation(&self) -> OperationNetworkIsolation {
        self.isolation
    }
    /// The order's `buyAmount`, also a Reshield post-hook's guard amount, for an approved
    /// minimum. It equals the minimum for External delivery.
    pub fn buy_amount_for(&self, private_minimum: U256) -> Result<U256> {
        Ok(order_buy_amount(private_minimum, self.shield_fee_bps)?)
    }

    /// The terms to persist when the user approves this review with its setup.
    pub fn approval(
        &self,
        private_minimum: U256,
        price_acknowledged: bool,
    ) -> Result<SwapApproval> {
        let destination_minimum = self.bridge.map(|bridge| bridge.destination_minimum);
        let buy_amount =
            self.require_approval(private_minimum, destination_minimum, price_acknowledged)?;
        Ok(SwapApproval {
            bounds: SwapApprovedBounds {
                sell_amount: self.sell_amount,
                unshield_amount: Some(self.plan.amount()),
                unshield_fee_bps: self.unshield_fee_bps,
                buy_amount,
                private_minimum,
                shield_fee_bps: self.shield_fee_bps,
                slippage_bps: self.slippage_bps,
                pre_hook_gas_limit: self.plan.pre_hook_gas_limit(),
                post_hook_gas_limit: self.plan.post_hook_gas_limit(),
                hook_cost: Some(self.hook_cost()),
                anchors: match &self.price {
                    SwapPrice::Verified { observations, .. } => observations.clone(),
                    SwapPrice::Unverified => Vec::new(),
                },
                destination_minimum,
            },
            price_verified: Some(self.price_verified()),
            price_acknowledged,
            delivery: self.plan.delivery,
            tokens: Some(SwapApprovalTokens {
                sell: self.plan.sell_token,
                buy: self.plan.buy_token,
            }),
        })
    }

    /// How this fresh review, planned for the approved amount and slippage once the setup is
    /// confirmed, differs from the approval. `None` means the order can be signed with the
    /// approved minimum: a better quote only adds surplus. Another delivery kind or receiver
    /// address, a lower suggested minimum or destination minimum, another Railgun fee the order
    /// depends on, or another kind of price check needs a new review.
    #[must_use]
    pub fn approval_change(&self, approval: &SwapApproval) -> Option<SwapReviewChange> {
        // Only the parsed address counts; a receiver's label isn't part of the delivery.
        if self.plan.delivery != approval.delivery {
            return Some(SwapReviewChange::Delivery);
        }
        let approved = &approval.bounds;
        // An External order carries no shield, so it doesn't depend on the shield fee.
        if matches!(self.plan.delivery, SwapDelivery::Reshield)
            && self.shield_fee_bps != approved.shield_fee_bps
        {
            return Some(SwapReviewChange::ShieldFee {
                approved: approved.shield_fee_bps,
                current: self.shield_fee_bps,
            });
        }
        if self.unshield_fee_bps != approved.unshield_fee_bps {
            return Some(SwapReviewChange::UnshieldFee {
                approved: approved.unshield_fee_bps,
                current: self.unshield_fee_bps,
            });
        }
        if approved
            .hook_cost
            .is_none_or(|cost| self.hook_cost() > cost)
            || self.plan.pre_hook_gas_limit() > approved.pre_hook_gas_limit
            || self.plan.post_hook_gas_limit().is_some_and(|current| {
                approved
                    .post_hook_gas_limit
                    .is_none_or(|limit| current > limit)
            })
        {
            return Some(SwapReviewChange::HookCost);
        }
        let was_verified = approval
            .price_verified
            .unwrap_or(!approved.anchors.is_empty());
        if self.price_verified() != was_verified {
            return Some(SwapReviewChange::PriceVerification);
        }
        let current = self.suggested_private_minimum();
        if current < approved.private_minimum {
            return Some(SwapReviewChange::Minimum {
                approved: approved.private_minimum,
                current,
            });
        }
        // A Bridge approval without a destination minimum binds none, so it signs nothing.
        let destination = self
            .bridge
            .map_or(U256::ZERO, |bridge| bridge.destination_minimum);
        if matches!(self.plan.delivery, SwapDelivery::Bridge(_))
            && approved
                .destination_minimum
                .is_none_or(|minimum| destination < minimum)
        {
            return Some(SwapReviewChange::DestinationMinimum {
                approved: approved.destination_minimum.unwrap_or_default(),
                current: destination,
            });
        }
        None
    }

    /// A Bridge delivery needs its bridge quote and a nonzero `destination_minimum`; other
    /// deliveries take none.
    fn require_approval(
        &self,
        private_minimum: U256,
        destination_minimum: Option<U256>,
        price_acknowledged: bool,
    ) -> Result<U256> {
        if let SwapDelivery::Bridge(bridge) = self.plan.delivery {
            if !bridge.has_valid_surplus() {
                return Err(eyre!("this bridge provider can't handle surplus that way"));
            }
            if self.bridge.is_none() {
                return Err(eyre!("quote the bridge before approving this swap"));
            }
            if destination_minimum.is_none_or(|minimum| minimum.is_zero()) {
                return Err(eyre!(
                    "approve a minimum received on the destination network for this swap"
                ));
            }
        } else if destination_minimum.is_some() {
            return Err(eyre!("only a bridge swap has a destination minimum"));
        }
        if !self.price_verified() && !price_acknowledged {
            return Err(eyre!(
                "acknowledge the unverified price before approving this swap"
            ));
        }
        self.buy_amount_for(private_minimum)
    }
}

pub struct SwapReviewRequest<'a> {
    pub plan: SwapInputPlan,
    pub slippage_bps: u32,
    /// The swap's own orderbook client, kept for its order submission.
    pub orderbook: &'a CowOrderbookClient,
    /// `None` when returning for unverified-price acknowledgement. Signing also uses the
    /// background cache, and still checks the downside limit if a rate becomes available.
    pub anchor_cache: Option<&'a TokenAnchorRateCache>,
    pub token_registry: &'a EffectiveTokenRegistry,
    /// Required for Bridge delivery, whose bridge leg the review quotes for the order's buy
    /// amount.
    pub bridge: Option<SwapBridgeRoute<'a>>,
}

/// Approval of a reviewed swap.
pub struct SwapOrderRequest<'a> {
    pub review: &'a SwapReview,
    /// `M`, the approved minimum received privately after the shield fee, or by an External
    /// receiver.
    pub private_minimum: U256,
    /// Required when the review's price is unverified.
    pub price_acknowledged: bool,
    pub session: Arc<WalletSession>,
    pub authorization: DesktopPrivateSpendAuthorization,
    pub orderbook: &'a CowOrderbookClient,
    pub anchor_cache: &'a TokenAnchorRateCache,
    pub token_registry: &'a EffectiveTokenRegistry,
    /// The route of a Bridge delivery: the provider to quote again while signing, and the
    /// destination chain whose contracts its receiver is checked against. Required for Bridge
    /// delivery.
    pub bridge: Option<SwapBridgeRoute<'a>>,
    /// The approved minimum received on a Bridge delivery's destination chain: the review's
    /// bridge quote minimum for a fresh approval, or the saved approval's. Required for Bridge
    /// delivery.
    pub destination_minimum: Option<U256>,
    pub verify_proof: bool,
}

/// A term that changed between review and signing. Nothing was signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapReviewChange {
    /// The delivery kind, the External receiver's address, or a Bridge delivery's provider,
    /// destination, receiver, token or surplus choice differs from the approval. This always
    /// needs a full review.
    Delivery,
    /// A displayed network/hook limit increased, or an older approval lacks it.
    HookCost,
    ShieldFee {
        approved: U256,
        current: U256,
    },
    /// Another unshield fee changes the order's sell amount. An approval from before orders
    /// sold the amount after that fee reports it as zero.
    UnshieldFee {
        approved: U256,
        current: U256,
    },
    /// The quote now deviates from cached anchors beyond the profile's threshold.
    QuoteDeviates,
    /// The pair's anchors became readable or stopped being configured.
    PriceVerification,
    /// A previously checked price is unavailable. Review again without cached verification.
    PriceUnavailable,
    /// The fresh quote no longer supports the minimum approved before setup.
    Minimum {
        approved: U256,
        current: U256,
    },
    /// A fresh bridge quote delivers less than the approved minimum on the destination chain.
    DestinationMinimum {
        approved: U256,
        current: U256,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapOrderOutcome {
    /// The attempt was persisted, then the orderbook accepted the order.
    Submitted { uid: OrderUid },
    /// Nothing was signed. The user must review the changed terms.
    ReviewRequired(SwapReviewChange),
    /// The app data doesn't fit. Offer the largest amount under `byte_budget` through
    /// [`ExecutorOwner::plan_swap_amount`]; the swap hasn't failed. With `attempt_recorded`, the
    /// rejected order was persisted and sent, and its attempt must end through tracking before
    /// another one is admitted.
    Replan {
        byte_budget: usize,
        attempt_recorded: bool,
    },
}

/// Map the orderbook's answer to a persisted order. A size rejection returns to the amount
/// offer with a budget below the rejected app data.
pub fn swap_submission_outcome(
    submitted: Result<OrderUid, CowApiError>,
    uid: OrderUid,
    app_data_len: usize,
    byte_budget: usize,
) -> Result<SwapOrderOutcome> {
    match submitted {
        Ok(assigned) if assigned == uid => Ok(SwapOrderOutcome::Submitted { uid }),
        Err(CowApiError::DuplicatedOrder) => Ok(SwapOrderOutcome::Submitted { uid }),
        Ok(_) => Err(eyre!(
            "the CoW orderbook assigned a different order UID than the one signed"
        )),
        Err(error) if error.requires_replan() => Ok(SwapOrderOutcome::Replan {
            byte_budget: swap_replan_budget(byte_budget, app_data_len),
            attempt_recorded: true,
        }),
        Err(error) => Err(error.into()),
    }
}

/// The next plan's budget after app data of `rejected_len` bytes didn't fit.
const fn swap_replan_budget(byte_budget: usize, rejected_len: usize) -> usize {
    let below = rejected_len.saturating_sub(1);
    if below < byte_budget {
        below
    } else {
        byte_budget
    }
}

/// Select `utxos` for a swap pre-hook and check it against the batch limit and `byte_budget`,
/// without proving. The pre-hook unshields to the executor and runs the deadline guard, an
/// optional invalidation of `invalidates`, and the exact approval. The app data holds a
/// post-hook only for Reshield and Across delivery. Hook calls are sized with their real
/// encodings; every argument that is only known at signing is a static ABI word.
pub(crate) fn plan_swap_inputs(
    builder: &TransactionBuilder,
    profile: &SwapProfile,
    executor: impl Into<SwapExecutor>,
    utxos: &[Utxo],
    request: &SwapAmountRequest,
    byte_budget: usize,
    invalidates: Option<OrderUid>,
) -> Result<SwapAmountPlan> {
    let executor = executor.into();
    let context = ExecutorContext {
        chain_id: builder.chain_id,
        executor: executor.executor(),
        delegate: executor.delegate(),
        execution_nonce: executor.expected_pre_hook_nonce(),
    };
    let calls = pre_hook_calls(
        profile,
        context.executor,
        request.sell_token,
        request.amount,
        u32::MAX,
        invalidates,
    )?;
    let gas_model = RailgunGasModel::for_chain(profile.chain_id());
    let post_hook_gas =
        swap_post_hook_gas(gas_model, request.delivery, GasEstimateMode::UpperBound);
    let template = SwapAppDataTemplate {
        app_code: profile.app_code().to_owned(),
        pre_hook_gas_limit: hook_gas_limit(pre_hook_gas(
            gas_model,
            &[WIDEST_PRE_HOOK_TRANSACTION; MAX_BATCH_TRANSACTIONS],
            PreHookCalls {
                invalidate_order: invalidates.is_some(),
            },
            GasEstimateMode::UpperBound,
        )),
        post_hook: match post_hook_gas {
            Some(gas) => Some(SwapPostHookTemplate {
                calls: placeholder_post_hook_calls(
                    context.executor,
                    request.buy_token,
                    request.delivery,
                )?,
                gas_limit: hook_gas_limit(gas),
            }),
            None => None,
        },
    };
    let check = builder.check_swap_pre_hook(
        utxos,
        &pre_hook_request(context, request.sell_token, request.amount, calls),
        &template,
        byte_budget,
    )?;
    let plan = |size| SwapInputPlan {
        executor,
        context,
        sell_token: request.sell_token,
        buy_token: request.buy_token,
        delivery: request.delivery,
        invalidates,
        byte_budget,
        size,
        gas_model,
        post_hook_gas,
    };
    Ok(match check {
        SwapAmountCheck::Fits(size) => SwapAmountPlan::Fits(plan(size)),
        SwapAmountCheck::TooLarge { largest } => SwapAmountPlan::TooLarge {
            largest: plan(largest),
        },
    })
}

/// Price the order limit for a hook-free quote of the order's sell amount, the plan's amount
/// after the unshield fee: hook gas, slippage, and for Reshield delivery the shield fee. External
/// and Bridge orders carry no shield of the buy amount, so their reviews use a shield fee of zero
/// and their buy amount is the approved minimum.
#[allow(clippy::too_many_arguments)]
pub(crate) fn price_swap_review(
    plan: SwapInputPlan,
    quote: CowQuote,
    price: SwapPrice,
    shield_fee_bps: U256,
    unshield_fee_bps: U256,
    slippage_bps: u32,
    gas_price_wei: u128,
    hook_data_cost_wei: U256,
    isolation: OperationNetworkIsolation,
) -> Result<SwapReview> {
    let sell_amount = order_sell_amount(plan.amount(), unshield_fee_bps)?;
    let surplus_shield_fee_bps = shield_fee_bps;
    let shield_fee_bps = match plan.delivery {
        SwapDelivery::Reshield => shield_fee_bps,
        SwapDelivery::External { .. } | SwapDelivery::Bridge(_) => U256::ZERO,
    };
    let native_rate = match &price {
        SwapPrice::Verified { rate, .. } => NativeBuyRate::Anchor(rate.buy_rate),
        SwapPrice::Unverified => NativeBuyRate::Quote,
    };
    let params = OrderLimitParams {
        quote: &quote.quote,
        hook_gas: plan.hook_gas_estimate(),
        gas_price_wei,
        hook_data_cost_wei,
        native_rate,
        slippage_bps,
        shield_fee_bps,
    };
    // The displayed estimate uses the current RPC price. The signed minimum, and thus an
    // Across deposit, allow a 25% increase, rounded upward to whole wei. Neither changes the
    // hooks' execution gas limits or applies the broadcaster's separate gas-price buffer.
    let limit_gas_price_wei = gas_price_wei
        .checked_add(gas_price_wei.div_ceil(4))
        .ok_or(OrderLimitError::Overflow)?;
    tracing::debug!(
        target: "swap_quote",
        step = "gas_price_comparison",
        cow_gas_price_wei = %quote.quote.gas_price,
        rpc_gas_price_wei = gas_price_wei,
        estimated_gas_price_wei = gas_price_wei,
        limit_gas_price_wei,
        "priced hooks from RPC gas"
    );
    let limit = price_order_limit(&OrderLimitParams {
        gas_price_wei: limit_gas_price_wei,
        ..params
    })
    .map_err(|error| match error {
        // Report the wallet's buy token, not CoW's native buy address.
        OrderLimitError::HookCostExceedsOutput {
            hook_cost,
            quoted_output,
            ..
        } => OrderLimitError::HookCostExceedsOutput {
            buy_token: plan.buy_token,
            hook_cost,
            quoted_output,
        },
        error => error,
    })?;
    let estimated_hook_cost = price_order_limit(&OrderLimitParams {
        hook_gas: plan.expected_hook_gas(),
        ..params
    })?
    .hook_cost;
    Ok(SwapReview {
        plan,
        quote: quote.quote,
        quote_id: quote.id,
        limit,
        estimated_hook_cost,
        surplus_shield_fee_bps,
        shield_fee_bps,
        unshield_fee_bps,
        sell_amount,
        slippage_bps,
        price,
        isolation,
        bridge: None,
    })
}

/// Commits sender-output PPOI contexts for the pre-hook's private change outputs before the
/// order request. The wallet actor persists them and hands them to the chain service, which
/// submits the PPOI when it observes the commitments in the settlement's `Transact`. Nothing
/// here names a transaction: whoever runs the pre-hook, the commitments identify the outputs.
#[async_trait::async_trait]
pub(crate) trait SwapOutputPoiSink: Sync {
    async fn commit(&self, contexts: &[PendingOutputPoiContextRecord]) -> Result<()>;
}

#[async_trait::async_trait]
impl SwapOutputPoiSink for WalletSession {
    async fn commit(&self, contexts: &[PendingOutputPoiContextRecord]) -> Result<()> {
        create_pending_output_poi_contexts(self, contexts)
            .await
            .map(drop)
    }
}

/// A reviewed swap's proved pre-hook and approval, ready to sign.
pub(crate) struct SwapOrderSigning<'a> {
    pub(crate) review: &'a SwapReview,
    pub(crate) private_minimum: U256,
    pub(crate) price_acknowledged: bool,
    /// Proved Railgun transactions of the pre-hook, unsigned.
    pub(crate) transactions: Vec<Transaction>,
    /// The notes whose nullifiers `transactions` spend.
    pub(crate) inputs: &'a [Utxo],
    /// PPOI contexts for the change outputs of freshly proved `transactions`. Empty when a
    /// retry reuses the proof: its contexts were committed with the first attempt.
    pub(crate) change_output_pois: Vec<PendingOutputPoiContextRecord>,
    pub(crate) output_pois: &'a dyn SwapOutputPoiSink,
    pub(crate) authorization: &'a DesktopPrivateSpendAuthorization,
    pub(crate) orderbook: &'a CowOrderbookClient,
    pub(crate) anchor_cache: &'a TokenAnchorRateCache,
    pub(crate) token_registry: &'a EffectiveTokenRegistry,
    /// Required for Bridge delivery: see [`SwapOrderRequest::bridge`].
    pub(crate) bridge: Option<SwapBridgeRoute<'a>>,
    /// Required for Bridge delivery: see [`SwapOrderRequest::destination_minimum`].
    pub(crate) destination_minimum: Option<U256>,
}

enum SwapRecheck {
    Current(Vec<SwapAnchorObservation>),
    Changed(SwapReviewChange),
}

impl ExecutorOwner {
    /// An orderbook client on a network route dedicated to one swap. Keep it for that swap's
    /// quotes and its order submission.
    pub async fn swap_orderbook_client(&self) -> Result<CowOrderbookClient> {
        let profile = self.swap_order_profile()?;
        let base_url = Url::parse(profile.orderbook_api_base())?;
        let http = self.while_active(self.http.operation_http_client()).await?;
        Ok(CowOrderbookClient::new(
            http,
            base_url,
            self.chain.chain_id,
        )?)
    }

    /// Plan the pre-hook's notes for `request` from POI-spendable notes that no other executor
    /// operation reserves. Nothing is proved or signed. An amount that doesn't fit yields the
    /// largest amount that does, to offer instead. `executor` may be a preview, a reserved
    /// executor whose setup isn't confirmed, or the delegated one; only a plan for the
    /// delegated executor can be signed.
    pub fn plan_swap_amount(
        &self,
        executor: &SwapExecutor,
        session: &WalletSession,
        request: &SwapAmountRequest,
    ) -> Result<SwapAmountPlan> {
        self.require_swap_session(session)?;
        let profile = self.swap_order_profile()?;
        if profile.pair_eligibility(request.sell_token, request.buy_token, request.delivery)
            != SwapTokenEligibility::Eligible
        {
            return Err(eyre!("this token pair is not eligible for private swaps"));
        }
        let chain = effective_desktop_chain_config(self.chain.chain_id, &self.chain)?;
        let builder = TransactionBuilder {
            chain_type: 0,
            chain_id: self.chain.chain_id,
            railgun_contract: chain.railgun_contract,
            relay_adapt_contract: chain.relay_adapt_contract,
        };
        let profile_budget = profile.app_data_byte_budget();
        let byte_budget = request
            .byte_budget
            .map_or(profile_budget, |budget| budget.min(profile_budget));
        let Some(operation) = executor.operation() else {
            // A preview spends only notes that no executor operation reserves.
            let utxos = self.filter_reserved_inputs(self.spendable_swap_notes(session)?, None)?;
            return plan_swap_inputs(
                &builder,
                &profile,
                *executor,
                &utxos,
                request,
                byte_budget,
                None,
            );
        };
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        if matches!(executor.setup, SwapExecutorSetup::Recorded) {
            swap_attempt_refusal(&record, || record.has_recorded_unresolved_issued_work())
                .map_or(Ok(()), |refusal| Err(eyre!(refusal)))?;
        } else {
            require_swap_attempt_admitted(&record)?;
        }
        // Without reuse, the executor places the pair it was set up for: the setup approval's
        // before the first order, then the last order's.
        let pair = match record.swap() {
            Some(swap) => Some((swap.terms().sell_token(), swap.terms().buy_token())),
            None => record.swap_approval_tokens(),
        };
        if record.address() != Some(executor.executor())
            || record.delegate() != executor.delegate()
            || !executor.is_reused() && pair != Some((request.sell_token, request.buy_token))
        {
            return Err(eyre!("this swap's executor was set up for other tokens"));
        }
        let utxos = self.swap_inputs(session, &record)?;
        plan_swap_inputs(
            &builder,
            &profile,
            *executor,
            &utxos,
            request,
            byte_budget,
            swap_invalidation(&record, &profile, SystemTime::now())?,
        )
    }

    /// Quote using the shared protocol fee and background anchor rates, without hooks, and
    /// price the order limit. Signing rechecks the fees and anchors before using these terms.
    pub async fn review_swap(&self, request: SwapReviewRequest<'_>) -> Result<SwapReview> {
        self.while_active(Box::pin(self.review_swap_active(request)))
            .await
    }

    async fn review_swap_active(&self, request: SwapReviewRequest<'_>) -> Result<SwapReview> {
        let SwapReviewRequest {
            plan,
            slippage_bps,
            orderbook,
            anchor_cache,
            token_registry,
            bridge,
        } = request;
        let profile = self.swap_order_profile()?;
        let bridge = bridge_route(&plan, bridge)?;
        if let Some(operation) = plan.operation() {
            self.swap_account_record(operation)?
                .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        }
        let unshield_fee_bps = RAILGUN_PROTOCOL_FEE_BPS;
        let shield_fee_bps = RAILGUN_PROTOCOL_FEE_BPS;
        let sell_amount = order_sell_amount(plan.amount(), unshield_fee_bps)?;
        let chain = effective_desktop_chain_config(self.chain.chain_id, &self.chain)?;
        let pool = query_rpc_pool_with_http_client(chain.rpc_urls, &self.http);
        // Before setup the quote names the reserved executor or, for a preview, a stand-in
        // with no code, just as a fresh executor has none.
        let quote = async {
            let started = Instant::now();
            tracing::debug!(target: "swap_quote", step = "cow_quote", "started");
            let quote = orderbook
                .quote_sell(&swap_quote_request(
                    &plan,
                    sell_amount,
                    valid_to_after(SystemTime::now(), profile.valid_to_window())?,
                ))
                .await;
            tracing::debug!(
                target: "swap_quote",
                step = "cow_quote",
                elapsed_ms = started.elapsed().as_millis(),
                success = quote.is_ok(),
                "finished"
            );
            quote.map_err(eyre::Report::from)
        };
        let gas_price = async {
            let started = Instant::now();
            tracing::debug!(target: "swap_quote", step = "gas_price", "started");
            let gas_price = gas_price_from_rpc_pool_with_policy(&pool, 1, 1).await;
            tracing::debug!(
                target: "swap_quote",
                step = "gas_price",
                elapsed_ms = started.elapsed().as_millis(),
                success = gas_price.is_ok(),
                "finished"
            );
            gas_price
        };
        let (quote, gas_price_wei, hook_data_cost_wei) = tokio::try_join!(
            quote,
            gas_price,
            hook_data_cost_from_rpc_pool(
                &pool,
                self.chain.chain_id,
                plan.app_data_len(),
                &chain.gas,
            ),
        )?;
        let started = Instant::now();
        tracing::debug!(target: "swap_quote", step = "price_anchors", "started");
        let anchor = anchor_cache.map_or(Ok(None), |cache| {
            cache.cached_pair_rate(
                self.chain.chain_id,
                plan.sell_token,
                anchor_token(self.chain.chain_id, plan.buy_token),
                token_registry,
            )
        });
        tracing::debug!(
            target: "swap_quote",
            step = "price_anchors",
            source = "cache",
            elapsed_ms = started.elapsed().as_millis(),
            status = match &anchor {
                Ok(Some(_)) => "available",
                Ok(None) => "unverified",
                Err(_) => "unavailable",
            },
            "finished"
        );
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
        let mut review = price_swap_review(
            plan,
            quote,
            price,
            shield_fee_bps,
            unshield_fee_bps,
            slippage_bps,
            gas_price_wei,
            hook_data_cost_wei,
            orderbook.isolation(),
        )?;
        if let Some((delivery, route)) = bridge {
            // Bridge orders carry no shield, so the buy amount is the suggested minimum.
            let started = Instant::now();
            tracing::debug!(target: "swap_quote", step = "bridge_quote", "started");
            let quote = self
                .quote_swap_bridge(
                    route,
                    delivery,
                    &profile,
                    review.limit.buy_amount,
                    slippage_bps,
                    anchor_cache,
                    token_registry,
                )
                .await;
            tracing::debug!(
                target: "swap_quote",
                step = "bridge_quote",
                elapsed_ms = started.elapsed().as_millis(),
                success = quote.is_ok(),
                "finished"
            );
            review.bridge = Some(quote?);
        }
        Ok(review)
    }

    /// Prove the planned pre-hook, or reuse the recorded proof when a retry spends the same
    /// notes for the same amount, then sign, persist, and submit the order.
    pub async fn submit_swap_order(
        &self,
        request: SwapOrderRequest<'_>,
    ) -> Result<SwapOrderOutcome> {
        static NEXT_PREPARATION: AtomicU64 = AtomicU64::new(1);
        let span = tracing::debug_span!(target: "executor_observation", "swap_order",
            preparation_id = NEXT_PREPARATION.fetch_add(1, Ordering::Relaxed));
        trace_step(
            "order_total",
            self.while_active(Box::pin(self.submit_swap_order_active(request))),
        )
        .instrument(span)
        .await
    }

    async fn submit_swap_order_active(
        &self,
        request: SwapOrderRequest<'_>,
    ) -> Result<SwapOrderOutcome> {
        let SwapOrderRequest {
            review,
            private_minimum,
            price_acknowledged,
            session,
            authorization,
            orderbook,
            anchor_cache,
            token_registry,
            bridge,
            destination_minimum,
            verify_proof,
        } = request;
        self.require_swap_session(&session)?;
        review.require_approval(private_minimum, destination_minimum, price_acknowledged)?;
        bridge_route(&review.plan, bridge)?;
        let confirmed = session
            .sync_tip_rx
            .borrow()
            .safe_head_block
            .ok_or_else(|| eyre!("waiting for the wallet to sync the chain head"))?;
        let mut review = review.clone();
        if let Some(change) = trace_step(
            "order_account_refresh",
            self.refresh_swap_executor(&mut review, confirmed),
        )
        .await?
        {
            return Ok(SwapOrderOutcome::ReviewRequired(change));
        }
        let review = &review;
        let plan = &review.plan;
        let delegated = require_delegated_plan(plan)?;
        let record = self
            .swap_account_record(delegated.operation())?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        // No proof is built for a retry the store would refuse.
        require_swap_attempt_admitted(&record)?;
        let utxos = self.swap_inputs(&session, &record)?;
        let (transactions, inputs, change_output_pois) = if let Some((transactions, inputs)) =
            reusable_swap_proof(&record, plan, &utxos)
        {
            tracing::debug!(target: "executor_observation", step = "order_proof", reused = true);
            (transactions, inputs, Vec::new())
        } else {
            trace_step(
                "order_proof",
                Box::pin(self.prove_swap_pre_hook(
                    plan,
                    &session,
                    &authorization,
                    &utxos,
                    verify_proof,
                )),
            )
            .await?
        };
        Box::pin(self.issue_swap_order(SwapOrderSigning {
            review,
            private_minimum,
            price_acknowledged,
            transactions,
            inputs: &inputs,
            change_output_pois,
            output_pois: session.as_ref(),
            authorization: &authorization,
            orderbook,
            anchor_cache,
            token_registry,
            bridge,
            destination_minimum,
        }))
        .await
    }

    async fn prove_swap_pre_hook(
        &self,
        plan: &SwapInputPlan,
        session: &WalletSession,
        authorization: &DesktopPrivateSpendAuthorization,
        utxos: &[Utxo],
        verify_proof: bool,
    ) -> Result<(
        Vec<Transaction>,
        Vec<Utxo>,
        Vec<PendingOutputPoiContextRecord>,
    )> {
        let profile = self.swap_order_profile()?;
        let chain = effective_desktop_chain_config(self.chain.chain_id, &self.chain)?;
        let builder = TransactionBuilder {
            chain_type: 0,
            chain_id: self.chain.chain_id,
            railgun_contract: chain.railgun_contract,
            relay_adapt_contract: chain.relay_adapt_contract,
        };
        let mut request = plan.proof_request(&profile)?;
        request.verify_proof = verify_proof;
        let source = artifact_source(&self.http, &session.db)?;
        let prover = ProverService::new_with_db(&source, &session.db);
        let chain_handle = session
            .sync_manager
            .chain_handle(&session.chain_key)
            .await
            .ok_or_else(|| eyre!("swap chain is unavailable"))?;
        let mut forest = trace_step("order_forest", async {
            Ok::<_, eyre::Report>(chain_handle.forest.read().await.clone())
        })
        .await?;
        forest.compute_roots();
        let signer = authorization.signer(&self.vault, &self.view, SWAP_SPEND_OPERATION)?;
        let proved = trace_step(
            "order_transaction_proof",
            builder.build_mixed_private_action_plan_with_signer(
                &self.view.scan_keys(),
                &signer,
                &forest,
                utxos,
                request,
                &prover,
            ),
        )
        .await
        .map_err(|error| match error {
            BuildError::PinnedInputUnavailable { .. }
            | BuildError::PinnedInputsChanged { .. }
            | BuildError::PinnedInputsInsufficient(_) => {
                eyre!("The funds selected for this swap changed. Review the quote again.")
            }
            BuildError::CompositePlanShapeChanged { .. } => {
                eyre!("The swap changed during preparation. Review the quote again.")
            }
            error => error.into(),
        })?;
        drop(signer);
        // Like any private spend, the pre-hook's change outputs need sender-output PPOI. Only
        // change is private; the sell amount is unshielded to the executor.
        let (poi_list_keys, pre_transaction_pois) = trace_step(
            "order_pre_transaction_poi",
            active_list_pre_transaction_pois(
                &proved.chunks,
                session,
                self.chain.chain_id,
                &prover,
                verify_proof,
                &self.http,
                "generate private swap pending output pre-transaction POI",
            ),
        )
        .await?;
        let change = proved
            .private_outputs
            .into_iter()
            .filter(|output| output.role == MixedPrivateOutputRole::Change)
            .collect::<Vec<_>>();
        let change_output_pois = build_pending_mixed_output_poi_context_records(
            self.chain.chain_id,
            &session.cache_key,
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            &proved.chunks,
            &change,
            &pre_transaction_pois,
            &poi_list_keys,
        )?;
        let transactions =
            RelayAdapt7702::executeCall::abi_decode(&proved.call.data)?._transactions;
        Ok((
            transactions,
            proved.inputs.into_iter().map(|input| input.utxo).collect(),
            change_output_pois,
        ))
    }

    /// Sign the pre-hook at the executor's current nonce `k`, for Reshield and Across delivery
    /// the post-hook at `k + 1`, and the order, after checking cached anchors. The first order
    /// after setup must match the pair and delivery approved with the setup, an External
    /// receiver must pass [`SwapProfile::check_receiver`], and a Bridge receiver the destination
    /// chain's [`crate::settings::BridgeProfile::check_receiver`]. A Bridge order's provider is
    /// quoted again before anything is signed: Across for the deposit's terms, NEAR Intents for
    /// a verified deposit address that the order pays. Persist them with the input reservation
    /// and the provider's terms, then submit. Nothing signed leaves the wallet before the write
    /// succeeds.
    pub(crate) async fn issue_swap_order(
        &self,
        signing: SwapOrderSigning<'_>,
    ) -> Result<SwapOrderOutcome> {
        let SwapOrderSigning {
            review,
            private_minimum,
            price_acknowledged,
            transactions,
            inputs,
            change_output_pois,
            output_pois,
            authorization,
            orderbook,
            anchor_cache,
            token_registry,
            bridge,
            destination_minimum,
        } = signing;
        let plan = &review.plan;
        let delegated = require_delegated_plan(plan)?;
        let (operation, executor) = (delegated.operation(), delegated.executor());
        let buy_amount =
            review.require_approval(private_minimum, destination_minimum, price_acknowledged)?;
        let bridge = bridge_route(plan, bridge)?;
        let profile = self.swap_order_profile()?;
        self.ensure_active()?;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        require_swap_attempt_admitted(&record)?;
        // The hooks are issued against the latest reconciled observation, which the write
        // below requires to be unchanged; the plan's proof shape fixes the nonce.
        let observed = record
            .nonce_observation()
            .filter(|observed| observed.nonce() == plan.context.execution_nonce)
            .ok_or_else(|| eyre!("the swap executor's nonce changed; plan the swap again"))?;
        if record.address() != Some(executor) || record.delegate() != delegated.delegate() {
            return Err(eyre!("the swap executor changed; plan the swap again"));
        }
        // The first order after setup places the approved pair and delivery. Later orders, with
        // their own reviews, aren't bound by it. A new delivery needs a new approval.
        if record.swap().is_none()
            && let Some(approval) = record.swap_approval()
        {
            if record.swap_approval_tokens() != Some((plan.sell_token, plan.buy_token)) {
                return Err(eyre!(
                    "this order's tokens differ from the ones approved with the swap's setup"
                ));
            }
            if approval.delivery != plan.delivery {
                return Ok(SwapOrderOutcome::ReviewRequired(SwapReviewChange::Delivery));
            }
        }
        if let SwapDelivery::External { receiver } = plan.delivery {
            profile.check_receiver(
                self.chain.require_railgun()?.deployment.contract,
                executor,
                receiver,
            )?;
        }
        if let Some((delivery, route)) = bridge {
            let destination = route.destination_chain;
            destination
                .bridge_profile()
                .ok_or_else(|| eyre!("the destination network doesn't support bridging"))?
                .check_receiver(
                    destination.require_railgun()?.deployment.contract,
                    delivery.receiver,
                )?;
        }
        // The pre-hook must invalidate every earlier order that could fill with its funds.
        if swap_invalidation(&record, &profile, SystemTime::now())?
            .is_some_and(|live| plan.invalidates != Some(live))
        {
            return Err(eyre!(
                "an earlier order of this swap can still fill; plan the swap again"
            ));
        }
        if transactions.len() != plan.transaction_count() {
            return Err(eyre!("the proved pre-hook differs from the planned notes"));
        }
        let nullifying_key = self.view.scan_keys().nullifying_key;
        require_input_nullifiers(
            &transactions,
            inputs
                .iter()
                .map(|input| (input.tree, B256::from(input.nullifier(nullifying_key))))
                .collect(),
        )?;
        self.ensure_active()?;
        let anchors =
            match self.recheck_swap_review(review, &profile, anchor_cache, token_registry)? {
                SwapRecheck::Current(anchors) => anchors,
                SwapRecheck::Changed(change) => {
                    return Ok(SwapOrderOutcome::ReviewRequired(change));
                }
            };

        let valid_to = valid_to_after(SystemTime::now(), profile.valid_to_window())?;
        // The provider quotes the approved order before anything is signed. A 1Click quote
        // names the receiver, so it follows every check above.
        let bridge_terms = match bridge {
            Some((delivery, route)) => {
                let destination_minimum = destination_minimum
                    .ok_or_else(|| eyre!("a bridge swap needs its approved destination minimum"))?;
                let signing = trace_step(
                    "order_bridge_quote",
                    self.while_active(self.bridge_signing_terms(
                        review,
                        route,
                        delivery,
                        buy_amount,
                        destination_minimum,
                        valid_to,
                        &profile,
                        anchor_cache,
                        token_registry,
                    )),
                )
                .await?;
                match signing {
                    BridgeSigning::Terms(terms) => Some(terms),
                    BridgeSigning::Changed(change) => {
                        return Ok(SwapOrderOutcome::ReviewRequired(change));
                    }
                }
            }
            None => None,
        };
        // Resolved before anything is signed.
        let receiver = order_receiver(executor, plan.delivery, bridge_terms.as_ref())?;

        let signing_started = Instant::now();
        tracing::debug!(target: "executor_observation", step = "order_sign", "started");
        let chain_id = self.chain.chain_id;
        let nonce = observed.nonce();
        let proof_digest = keccak256(transactions.abi_encode());
        let unsigned = TransactionCall {
            to: executor,
            data: RelayAdapt7702::executeCall {
                _transactions: transactions,
                _actionData: RelayAdapt7702ActionData {
                    requireSuccess: true,
                    minGasLimit: U256::ZERO,
                    calls: pre_hook_calls(
                        &profile,
                        executor,
                        plan.sell_token,
                        review.sell_amount,
                        valid_to,
                        plan.invalidates,
                    )?,
                },
                _nonce: nonce,
                _signature: Bytes::new(),
            }
            .abi_encode()
            .into(),
        };
        let pre_hook_hash = plan.context.signing_hash(&unsigned)?;
        let signer = self.authorized_executor_signer(
            authorization,
            &HardwareExecutorAction::Execute(operation),
            operation,
            record.index(),
        )?;
        if signer.address() != executor {
            return Err(eyre!("executor signing identity does not match the swap"));
        }
        let pre_hook = plan
            .context
            .authorize_call(&unsigned, signer.sign_hash_sync(&pre_hook_hash)?)?
            .data;
        let recipient = self.view.scan_keys().address_data();
        // Every post-hook shield of this swap shields the full buy-token balance to the
        // wallet's own address, keyed like executor recovery's shields.
        let wallet_shield = || -> Result<ShieldRequest> {
            let shield_key = Zeroizing::new(derive_shield_private_key(&Zeroizing::new(
                signer.to_bytes().0,
            ))?);
            Ok(build_shield_request(
                recipient.master_public_key,
                &recipient.viewing_public_key,
                TokenData::erc20(plan.buy_token),
                U120::ZERO,
                &shield_key,
            )?)
        };
        // A Reshield post-hook shields the bought token and an Across post-hook deposits it,
        // shielding any surplus when the user chose to. External and NEAR Intents orders pay
        // their receiver and sign none.
        let post_hook_calls = match (plan.delivery, &bridge_terms) {
            (SwapDelivery::Reshield, _) => Some(guarded_shield_calls(
                executor,
                plan.buy_token,
                buy_amount,
                wallet_shield()?,
            )?),
            (SwapDelivery::Bridge(delivery), Some(BridgeOrderTerms::Across(terms))) => {
                Some(bridge_deposit_calls(
                    executor,
                    terms.spoke_pool,
                    across_deposit(executor, delivery, terms),
                    (delivery.surplus == BridgeSurplus::Reshield)
                        .then(wallet_shield)
                        .transpose()?,
                )?)
            }
            (SwapDelivery::External { .. } | SwapDelivery::Bridge(_), _) => None,
        };
        let post_hook = match post_hook_calls {
            Some(calls) => {
                let post_hook_nonce = nonce
                    .checked_add(U256::ONE)
                    .ok_or_else(|| eyre!("executor nonce is exhausted"))?;
                let gas_limit = plan
                    .post_hook_gas_limit()
                    .ok_or_else(|| eyre!("the swap's post-hook is unavailable"))?;
                let hash = post_hook_signing_hash(&calls, post_hook_nonce, chain_id, executor);
                let calldata = signed_post_hook_calldata(
                    calls,
                    post_hook_nonce,
                    chain_id,
                    executor,
                    &signer.sign_hash_sync(&hash)?,
                )?;
                Some((post_hook_nonce, hash, calldata, gas_limit))
            }
            None => None,
        };
        let app_data = swap_app_data(
            profile.app_code(),
            executor,
            pre_hook.clone(),
            plan.pre_hook_gas_limit(),
            post_hook
                .as_ref()
                .map(|(_, _, calldata, gas_limit)| (calldata.clone(), *gas_limit)),
        )?;
        if app_data.document.len() > plan.byte_budget {
            // The plan's estimate is an upper bound, so the plan is stale. Nothing is kept.
            return Ok(SwapOrderOutcome::Replan {
                byte_budget: swap_replan_budget(plan.byte_budget, app_data.document.len()),
                attempt_recorded: false,
            });
        }
        let order = swap_order(
            plan.sell_token,
            plan.buy_token,
            receiver,
            review.sell_amount,
            buy_amount,
            valid_to,
            app_data.hash,
        );
        let digest = order_digest(&order, chain_id, profile.settlement());
        let signature = eip712_order_signature(&signer.sign_hash_sync(&digest)?);
        drop(signer);
        let uid = OrderUid::new(digest, executor, valid_to);
        tracing::debug!(target: "executor_observation", step = "order_sign",
            elapsed_ms = signing_started.elapsed().as_millis(), "finished");

        let input_identities = inputs
            .iter()
            .map(ExecutorInputIdentity::from_utxo)
            .collect::<Vec<_>>();
        // The change outputs' PPOI contexts are committed before the attempt becomes durable,
        // so a failed commit leaves no attempt that must wait for expiry.
        if !change_output_pois.is_empty() {
            trace_step(
                "order_output_poi_commit",
                self.while_active(output_pois.commit(&change_output_pois)),
            )
            .await?;
        }
        self.ensure_active()?;
        let guard = self.lock_activity().await;
        self.require_record_unchanged(&record)?;
        trace_step("order_persist", async {
            self.store.record_swap_attempt(
                operation,
                SwapAttempt {
                    terms: SwapTerms::new(
                        plan.sell_token,
                        plan.buy_token,
                        SwapRecipient::new(
                            recipient.master_public_key,
                            recipient.viewing_public_key,
                        ),
                        delegated.setup_payload(),
                    ),
                    proof: SwapProof::new(proof_digest, input_identities.clone()),
                    uid,
                    submission: Some(SwapSubmission::new(signature, review.quote_id)),
                    delivery: plan.delivery,
                    bounds: SwapApprovedBounds {
                        sell_amount: review.sell_amount,
                        unshield_amount: Some(plan.amount()),
                        unshield_fee_bps: review.unshield_fee_bps,
                        buy_amount,
                        private_minimum,
                        shield_fee_bps: review.shield_fee_bps,
                        slippage_bps: review.slippage_bps,
                        pre_hook_gas_limit: plan.pre_hook_gas_limit(),
                        post_hook_gas_limit: post_hook
                            .as_ref()
                            .map(|(_, _, _, gas_limit)| *gas_limit),
                        hook_cost: Some(review.hook_cost()),
                        anchors,
                        destination_minimum,
                    },
                    invalidates: plan.invalidates,
                    pre_hook: IssuedExecutorPayload::new(
                        nonce,
                        delegated.delegate(),
                        pre_hook_hash,
                        ExecutorPayloadPurpose::SwapPreHook,
                        ExecutorPayloadContext::new(pre_hook.clone(), observed, input_identities),
                    ),
                    post_hook: post_hook.map(|(post_hook_nonce, hash, calldata, _)| {
                        IssuedExecutorPayload::new(
                            post_hook_nonce,
                            delegated.delegate(),
                            hash,
                            ExecutorPayloadPurpose::SwapPostHook,
                            ExecutorPayloadContext::new(calldata, observed, Vec::new()),
                        )
                    }),
                    bridge: bridge_terms,
                },
            )
        })
        .await?;
        self.notify_change();
        drop(guard);

        // The orderbook request carries the signed hooks, so it follows the durable write.
        let submitted = self
            .while_active(async {
                Ok(trace_step(
                    "order_submit",
                    orderbook.submit_order(&CowOrderSubmission {
                        order: &order,
                        owner: executor,
                        signature: &signature,
                        app_data: &app_data,
                        quote_id: review.quote_id,
                    }),
                )
                .await)
            })
            .await?;
        self.record_swap_submission_result(operation, uid, &submitted)?;
        self.diagnose_unfunded_rejection(
            &submitted,
            profile.hooks_trampoline(),
            executor,
            &pre_hook,
            plan.pre_hook_gas_limit(),
        )
        .await?;
        swap_submission_outcome(submitted, uid, app_data.document.len(), plan.byte_budget)
    }

    /// Resend the original signed order without signing or authorizing new terms.
    /// Use this swap's isolated client, retained by the view or created after restart.
    pub async fn resubmit_swap_order(
        &self,
        operation: ExecutorOperationId,
        orderbook: &CowOrderbookClient,
    ) -> Result<SwapOrderOutcome> {
        let guard = self.lock_activity().await;
        self.ensure_active()?;
        let record = self
            .swap_record(operation)?
            .ok_or_else(|| eyre!("swap is unavailable"))?;
        let swap = record
            .swap()
            .ok_or_else(|| eyre!("swap order is unavailable"))?;
        let saved = swap
            .orders()
            .last()
            .ok_or_else(|| eyre!("swap order is unavailable"))?;
        if saved.submission_status() == SwapSubmissionStatus::Accepted {
            return Ok(SwapOrderOutcome::Submitted { uid: saved.uid() });
        }
        if saved.submission_status() == SwapSubmissionStatus::Rejected {
            return Err(eyre!(
                "the order was rejected; wait for its attempt to end before retrying"
            ));
        }
        if !matches!(
            super::observation::swap_order_state(saved),
            super::observation::SwapOrderState::Open
                | super::observation::SwapOrderState::PreHookOnly { expired: false }
        ) {
            return Err(eyre!("this order no longer awaits submission"));
        }
        // Leave at least one request timeout for the existing order to be accepted.
        if saved.valid_to() <= valid_to_after(SystemTime::now(), Duration::from_mins(1))? {
            return Err(eyre!(
                "the order is too close to expiry; wait for final expiry, then retry"
            ));
        }
        let submission = saved.submission().ok_or_else(|| {
            eyre!("this saved order has no resubmission signature; wait for expiry")
        })?;
        let profile = self.swap_order_profile()?;
        let executor = saved.uid().owner();
        let payload = |hash| {
            record
                .issued()
                .iter()
                .find(|payload| payload.hash() == hash)
                .map(|payload| payload.context().calldata().clone())
                .ok_or_else(|| eyre!("the saved swap hook is unavailable"))
        };
        let bounds = saved.bounds();
        let pre_hook = payload(saved.pre_hook().payload())?;
        let post_hook = match saved.post_hook() {
            Some(post_hook) => Some((
                payload(post_hook.payload())?,
                bounds
                    .post_hook_gas_limit
                    .ok_or_else(|| eyre!("the saved swap hook is unavailable"))?,
            )),
            None => None,
        };
        let app_data = swap_app_data(
            profile.app_code(),
            executor,
            pre_hook.clone(),
            bounds.pre_hook_gas_limit,
            post_hook,
        )?;
        let order = swap_order(
            swap.order_terms(saved).sell_token(),
            swap.order_terms(saved).buy_token(),
            order_receiver(executor, saved.delivery(), saved.bridge())?,
            bounds.sell_amount,
            bounds.buy_amount,
            saved.valid_to(),
            app_data.hash,
        );
        let uid = OrderUid::new(
            order_digest(&order, self.chain.chain_id, profile.settlement()),
            executor,
            saved.valid_to(),
        );
        if uid != saved.uid() {
            return Err(eyre!(
                "the saved order can't be reconstructed unchanged; wait for expiry"
            ));
        }
        // Sending the order again reserves notes the user released from it.
        match self
            .store
            .reserve_released(operation, saved.pre_hook().payload())
        {
            Ok(_) => {}
            Err(ExecutorStoreError::InputReserved) => {
                return Err(eyre!(
                    "another operation now uses this order's notes; wait for the order to expire"
                ));
            }
            Err(error) => return Err(error.into()),
        }
        self.notify_change();
        drop(guard);
        let result = self
            .while_active(async {
                Ok(orderbook
                    .submit_order(&CowOrderSubmission {
                        order: &order,
                        owner: executor,
                        signature: submission.signature(),
                        app_data: &app_data,
                        quote_id: submission.quote_id(),
                    })
                    .await)
            })
            .await?;
        self.record_swap_submission_result(operation, uid, &result)?;
        self.diagnose_unfunded_rejection(
            &result,
            profile.hooks_trampoline(),
            executor,
            &pre_hook,
            bounds.pre_hook_gas_limit,
        )
        .await?;
        swap_submission_outcome(
            result,
            uid,
            app_data.document.len(),
            app_data.document.len(),
        )
    }

    /// `CoW`'s trampoline ignores a reverting pre-hook, so the orderbook reports the account
    /// as unfunded without the reason. Only after that rejection, simulate the pre-hook to
    /// report why. Any other result, or a simulation without a failure, is left to the caller.
    async fn diagnose_unfunded_rejection(
        &self,
        result: &Result<OrderUid, CowApiError>,
        trampoline: Address,
        executor: Address,
        pre_hook: &Bytes,
        gas_limit: u64,
    ) -> Result<()> {
        let Err(CowApiError::Rejected { error_type, .. }) = result else {
            return Ok(());
        };
        if !matches!(
            error_type.as_str(),
            "InsufficientBalance" | "InsufficientAllowance"
        ) {
            return Ok(());
        }
        let simulation = trace_step(
            "order_pre_hook_simulation",
            self.while_active(async {
                Ok(simulate_pre_hook(
                    &self.endpoints,
                    trampoline,
                    executor,
                    pre_hook,
                    gas_limit,
                    self.chain.chain_id,
                )
                .await)
            }),
        )
        .await?;
        if let PreHookSimulation::Failed(reason) = simulation {
            return Err(eyre!(
                "CoW rejected the order because its pre-hook fails: {reason}"
            ));
        }
        Ok(())
    }

    fn record_swap_submission_result(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        result: &Result<OrderUid, CowApiError>,
    ) -> Result<()> {
        let status = match result {
            Ok(assigned) if *assigned == uid => SwapSubmissionStatus::Accepted,
            Err(CowApiError::DuplicatedOrder) => SwapSubmissionStatus::Accepted,
            Err(
                CowApiError::AppDataTooLarge
                | CowApiError::SellAmountDoesNotCoverFee
                | CowApiError::NoLiquidity
                | CowApiError::UnsupportedToken
                | CowApiError::Rejected { .. },
            ) => SwapSubmissionStatus::Rejected,
            _ => SwapSubmissionStatus::Pending,
        };
        self.ensure_active()?;
        self.store.record_swap_submission(operation, uid, status)?;
        self.notify_change();
        Ok(())
    }

    /// Check the pair's cached anchors right before signing. Railgun's fees are the shared
    /// protocol constant, as for every other operation, so nothing is read from the chain. An
    /// External order depends only on the unshield fee.
    fn recheck_swap_review(
        &self,
        review: &SwapReview,
        profile: &SwapProfile,
        anchor_cache: &TokenAnchorRateCache,
        token_registry: &EffectiveTokenRegistry,
    ) -> Result<SwapRecheck> {
        let plan = &review.plan;
        let started = Instant::now();
        let anchor = anchor_cache
            .cached_pair_rate(
                self.chain.chain_id,
                plan.sell_token,
                anchor_token(self.chain.chain_id, plan.buy_token),
                token_registry,
            )
            .ok()
            .flatten();
        tracing::debug!(target: "executor_observation", step = "order_price_anchors",
            source = "cache", elapsed_ms = started.elapsed().as_millis(),
            available = anchor.is_some(), "finished");
        recheck_swap_price(
            &review.quote,
            &review.price,
            anchor,
            profile.anchor_deviation_bps(),
        )
    }

    /// POI-spendable notes, without other operations' reservations or notes this swap's
    /// executor already spent.
    fn swap_inputs(&self, session: &WalletSession, record: &ExecutorRecord) -> Result<Vec<Utxo>> {
        self.inputs_for_record(self.spendable_swap_notes(session)?, record)
    }

    /// Largest batched sell amount currently available to this swap. The same POI,
    /// pending-spend and executor reservation filters apply as during planning.
    pub fn max_swap_amount(
        &self,
        session: &WalletSession,
        operation: Option<ExecutorOperationId>,
        token: Address,
    ) -> Result<U256> {
        self.require_swap_session(session)?;
        let inputs = self.spendable_swap_inputs(session, operation)?;
        Ok(crate::max_unshield_spendable(&inputs, token))
    }

    pub(super) fn spendable_swap_inputs(
        &self,
        session: &WalletSession,
        operation: Option<ExecutorOperationId>,
    ) -> Result<Vec<Utxo>> {
        match operation {
            Some(operation) => {
                let record = self
                    .swap_account_record(operation)?
                    .ok_or_else(|| eyre!("swap executor is unavailable"))?;
                self.swap_inputs(session, &record)
            }
            None => self.filter_reserved_inputs(self.spendable_swap_notes(session)?, None),
        }
    }

    #[cfg_attr(not(feature = "test-support"), allow(clippy::unused_self))]
    fn spendable_swap_notes(&self, session: &WalletSession) -> Result<Vec<Utxo>> {
        #[cfg(feature = "test-support")]
        {
            let notes = self
                .swap_notes_for_tests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !notes.is_empty() {
                return Ok(notes.clone());
            }
        }
        let snapshot = session
            .handle
            .current_snapshot()
            .ok_or_else(|| eyre!("private wallet snapshot is unavailable"))?;
        Ok(crate::poi_verified_unspent_utxos_from_records(
            &snapshot.utxos,
            &snapshot.pending_overlay,
        ))
    }

    /// Plan swaps from one spendable note holding `amount` of `token` instead of the session's
    /// notes. UI tests use it to quote through the real planning and review, since their
    /// sessions never sync.
    #[cfg(feature = "test-support")]
    pub fn plan_swaps_from_note_for_tests(&self, token: Address, amount: U256) {
        let note = Utxo::new(
            railgun_wallet::Note::new_change(U256::ONE, token, amount, [7; 16]),
            0,
            0,
            railgun_wallet::UtxoSource {
                tx_hash: B256::repeat_byte(6),
                block_number: 1,
                block_timestamp: 1,
            },
            railgun_wallet::UtxoCommitmentKind::Transact,
        );
        *self
            .swap_notes_for_tests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = vec![note];
    }

    fn require_swap_session(&self, session: &WalletSession) -> Result<()> {
        if !session
            .executor_owner()
            .is_some_and(|owner| std::ptr::eq(owner.as_ref(), self))
        {
            return Err(eyre!("swap wallet belongs to another session"));
        }
        Ok(())
    }

    fn swap_order_profile(&self) -> Result<SwapProfile> {
        self.swap_executor_profile()?;
        self.chain
            .swap_profile()
            .ok_or_else(|| eyre!("private swaps are unavailable on this chain"))
    }
}

impl ExecutorOwner {
    /// Quote an account whose setup is recorded, without waiting for chain observation.
    /// Reuse permits another token pair; execution preparation checks the account afresh.
    pub fn swap_order_preview(
        &self,
        operation: ExecutorOperationId,
        reuse: bool,
    ) -> Result<SwapExecutor> {
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("stealth account is unavailable"))?;
        if reuse {
            swap_account_candidate(&record, self.chain.chain_id)
                .ok_or_else(|| eyre!("this account is unavailable for another swap"))?;
        } else {
            swap_attempt_refusal(&record, || record.has_recorded_unresolved_issued_work())
                .map_or(Ok(()), |refusal| Err(eyre!(refusal)))?;
            if !super::is_swap_record(&record)
                || !record.issued().iter().any(|payload| {
                    payload.purpose() == ExecutorPayloadPurpose::Operation
                        && record.recorded_payload_status(payload.hash())
                            == Some(ExecutorPayloadStatus::Executed)
                })
            {
                return Err(eyre!("this swap's setup is not confirmed"));
            }
        }
        self.swap_order_profile()?;
        ExecutorProfile::accepted(self.chain.chain_id, record.delegate())
            .ok_or_else(|| eyre!("this swap executor's delegate is not supported"))?;
        Ok(SwapExecutor {
            operation: Some(operation),
            executor: record
                .address()
                .ok_or_else(|| eyre!("stealth account is unavailable"))?,
            delegate: record.delegate(),
            // The nonce is a fixed ABI word and does not affect the quote's gas or byte
            // estimate. Preparation replaces this hint before proving or signing.
            expected_pre_hook_nonce: record
                .nonce_observation()
                .map_or(U256::ZERO, ExecutorNonceObservation::nonce),
            setup: SwapExecutorSetup::Recorded,
            reused: reuse,
        })
    }

    /// Refresh mutable account state before proving. The executor nonce is not a priced
    /// term, but an additional order invalidation changes the hook estimate and needs review.
    pub(crate) async fn refresh_swap_executor(
        &self,
        review: &mut SwapReview,
        confirmed: u64,
    ) -> Result<Option<SwapReviewChange>> {
        let plan = &review.plan;
        if plan.executor.requires_setup() {
            return Err(eyre!(
                "the swap's stealth account needs setup before placing an order"
            ));
        }
        let operation = plan
            .operation()
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        let mut executor = self.reuse_swap_account(operation, confirmed).await?;
        if executor.executor() != plan.executor() || executor.delegate() != plan.context.delegate {
            return Err(eyre!("the swap executor changed; plan the swap again"));
        }
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("swap executor is unavailable"))?;
        if swap_invalidation(&record, &self.swap_order_profile()?, SystemTime::now())?
            .is_some_and(|live| plan.invalidates != Some(live))
        {
            return Ok(Some(SwapReviewChange::HookCost));
        }
        executor.reused = plan.executor.is_reused();
        review.plan.context.execution_nonce = executor.expected_pre_hook_nonce();
        // The quote used a nonce hint. Pin proving to the refreshed context while keeping
        // its reviewed inputs and all transaction-shape constraints unchanged.
        review.plan.size.preview.shape.execution =
            CompositeExecution::Executor(review.plan.context);
        review.plan.executor = executor;
        Ok(None)
    }

    /// Check the selected account and its prior orders before preparing execution.
    /// This does not sign, reserve inputs, or restart a stopped setup.
    pub async fn reuse_swap_account(
        &self,
        operation: ExecutorOperationId,
        confirmed: u64,
    ) -> Result<SwapExecutor> {
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("stealth account is unavailable"))?;
        if record.settled_swaps_at(confirmed) {
            require_swap_attempt_admitted(&record)?;
            let mut executor =
                trace_step("reuse_finalized", self.refresh_settled_swap(&record)).await?;
            executor.reused = true;
            return Ok(executor);
        }
        let range = confirmed..confirmed.saturating_add(1);
        let report = if record.swap().is_some() {
            trace_step("reuse_orders", self.observe_swap(operation, range)).await?
        } else {
            trace_step("reuse_history", self.reconcile_history(operation, range)).await?
        };
        let super::SwapSetupStatus::Delegated(delegated) =
            trace_step("reuse_setup", self.check_swap_setup(&report)).await?
        else {
            return Err(eyre!(
                "this account's setup is not confirmed; finish or retry its setup first"
            ));
        };
        let record = report.record();
        let _guard = self.lock_activity().await;
        self.require_record_unchanged(record)?;
        require_swap_attempt_admitted(record)?;
        if record.swap().is_none() && record.has_unresolved_issued_work() {
            return Err(eyre!(UNFINISHED_WORK));
        }
        let mut executor = SwapExecutor::from(delegated);
        executor.reused = true;
        Ok(executor)
    }

    /// Set-up stealth accounts the swap form can offer for another swap, hidden ones included.
    /// This reads local records only: a setup recorded as executed for an accepted delegate,
    /// and the local rules of [`Self::reuse_swap_account`] judged from recorded outcomes.
    /// Execution preparation observes and checks the chosen account with
    /// [`Self::reuse_swap_account`].
    pub fn swap_account_candidates(&self) -> Result<Vec<SwapAccountCandidate>> {
        self.swap_order_profile()?;
        let chain_id = self.chain.chain_id;
        Ok(self
            .records()?
            .iter()
            .filter_map(|record| swap_account_candidate(record, chain_id))
            .collect())
    }
}

/// A stealth account whose setup is done and that may place another swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapAccountCandidate {
    operation: ExecutorOperationId,
    index: u32,
    address: Address,
    hidden: bool,
    last_pair: Option<(Address, Address)>,
}

impl SwapAccountCandidate {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }
    #[must_use]
    pub const fn address(&self) -> Address {
        self.address
    }
    #[must_use]
    pub const fn is_hidden(&self) -> bool {
        self.hidden
    }
    /// The sell and buy tokens of the account's last swap.
    #[must_use]
    pub const fn last_pair(&self) -> Option<(Address, Address)> {
        self.last_pair
    }
}

fn swap_account_candidate(record: &ExecutorRecord, chain_id: u64) -> Option<SwapAccountCandidate> {
    let address = record.address()?;
    let set_up = ExecutorProfile::accepted(chain_id, record.delegate()).is_some()
        && record.issued().iter().any(|payload| {
            payload.purpose() == ExecutorPayloadPurpose::Operation
                && record.recorded_payload_status(payload.hash())
                    == Some(ExecutorPayloadStatus::Executed)
        });
    let unresolved = || record.has_recorded_unresolved_issued_work();
    // An approved order still awaiting placement belongs to its own swap.
    if !set_up
        || swap_attempt_refusal(record, unresolved).is_some()
        || record.swap().is_none() && (unresolved() || record.swap_approval().is_some())
    {
        return None;
    }
    Some(SwapAccountCandidate {
        operation: record.operation(),
        index: record.index(),
        address,
        hidden: record.is_hidden(),
        last_pair: record
            .swap()
            .map(|swap| (swap.terms().sell_token(), swap.terms().buy_token())),
    })
}

/// Hooks are signed only for a plan made for the confirmed delegation, at the nonce that
/// delegation was observed with. A recorded preview must be refreshed before proving.
fn require_delegated_plan(plan: &SwapInputPlan) -> Result<DelegatedSwapExecutor> {
    plan.executor.delegated().ok_or_else(|| {
        eyre!("the swap's stealth account isn't confirmed as set up; plan the swap again")
    })
}

/// A retry is refused while an earlier attempt's pre-hook can still run, including after it
/// executed without a trade: that order can still fill until `validTo`.
fn require_swap_attempt_admitted(record: &ExecutorRecord) -> Result<()> {
    swap_attempt_refusal(record, || record.has_unresolved_issued_work())
        .map_or(Ok(()), |refusal| Err(eyre!(refusal)))
}

const UNFINISHED_WORK: &str =
    "this account still has unfinished work; resolve it before starting a swap";
const PREVIOUS_ORDER_LIVE: &str =
    "the previous order of this swap can still execute; retry once it has ended";

/// Why `record` can't place a swap, from local state. `unresolved_work` reports signed work
/// that isn't settled, which blocks an account that doesn't belong to a swap.
fn swap_attempt_refusal(
    record: &ExecutorRecord,
    unresolved_work: impl FnOnce() -> bool,
) -> Option<&'static str> {
    if record.is_retired() && record.swap().is_none()
        || record.is_swap_setup_stopped()
        || record.public_account_uuid().is_some()
    {
        return Some("this account is retained for recovery or public use and cannot place a swap");
    }
    if !super::is_swap_record(record) && unresolved_work() {
        return Some(UNFINISHED_WORK);
    }
    if let Some(swap) = record.swap().filter(|swap| !swap.admits_attempt()) {
        return Some(
            swap.orders()
                .iter()
                .find_map(bridge_refusal)
                .unwrap_or(PREVIOUS_ORDER_LIVE),
        );
    }
    None
}

/// Why a Bridge order keeps its account from placing a swap, when its bridge is the reason.
fn bridge_refusal(order: &SwapOrderRecord) -> Option<&'static str> {
    match super::observation::swap_order_state(order) {
        super::observation::SwapOrderState::Bridging => {
            Some("the previous swap's bridge hasn't delivered yet; retry once it has")
        }
        super::observation::SwapOrderState::Refunding => Some(
            "the previous swap's bridge is refunding to this account; recover the funds instead",
        ),
        super::observation::SwapOrderState::NeedsAttention => {
            Some("the previous swap's bridge needs attention; check its status first")
        }
        _ => None,
    }
}

/// The earlier order of this executor that a retry's pre-hook invalidates in the same execution
/// that funds the executor. The pre-hook invalidates at most one order, so a retry waits while
/// two could still fill.
pub(crate) fn swap_invalidation(
    record: &ExecutorRecord,
    profile: &SwapProfile,
    now: SystemTime,
) -> Result<Option<OrderUid>> {
    match fillable_swap_orders(record, profile, now).as_slice() {
        [] => Ok(None),
        [uid] => Ok(Some(*uid)),
        _ => Err(eyre!(
            "two earlier orders of this swap can still fill; retry once one has expired"
        )),
    }
}

/// Orders of this executor that can still fill. An order can't once it traded, a finalized
/// block passed its `validTo`, or a payload that won its nonce invalidated it, such as a
/// cancellation. Orders within one validity window of local time count as live, to tolerate
/// clock skew.
pub(super) fn fillable_swap_orders(
    record: &ExecutorRecord,
    profile: &SwapProfile,
    now: SystemTime,
) -> Vec<OrderUid> {
    let Some(swap) = record.swap() else {
        return Vec::new();
    };
    let horizon = now
        .checked_sub(profile.valid_to_window())
        .and_then(|horizon| horizon.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |horizon| horizon.as_secs());
    swap.orders()
        .iter()
        .filter(|order| {
            let observed = order.observations();
            observed.traded.is_none()
                && observed.expired.is_none()
                && observed
                    .pre_hook_dead
                    .is_none_or(|death| death.cause != SwapPreHookDeathCause::Expired)
                && u64::from(order.valid_to()) >= horizon
                && !invalidated_by_winner(record, profile.settlement(), order.uid())
        })
        .map(SwapOrderRecord::uid)
        .collect()
}

/// Whether a reconciled winning payload of the executor called `invalidateOrder(uid)`.
fn invalidated_by_winner(record: &ExecutorRecord, settlement: Address, uid: OrderUid) -> bool {
    record.issued().iter().any(|payload| {
        if record.payload_status(payload.hash()) != Some(ExecutorPayloadStatus::Executed) {
            return false;
        }
        let data = payload.context().calldata();
        let calls = RelayAdapt7702::executeCall::abi_decode(data)
            .map(|call| call._actionData.calls)
            .or_else(|_| RelayAdapt7702::multicallCall::abi_decode(data).map(|call| call._calls));
        calls.is_ok_and(|calls| {
            calls.iter().any(|call| {
                call.to == settlement
                    && GPv2Settlement::invalidateOrderCall::abi_decode(&call.data)
                        .is_ok_and(|call| call.orderUid[..] == uid.0[..])
            })
        })
    })
}

/// A retry keeps the recorded proof while it spends the same notes for the same amount, the
/// private spend its pre-hook unshields. Returns the latest pre-hook's proved transactions
/// and the notes they spend.
pub(crate) fn reusable_swap_proof(
    record: &ExecutorRecord,
    plan: &SwapInputPlan,
    utxos: &[Utxo],
) -> Option<(Vec<Transaction>, Vec<Utxo>)> {
    let swap = record.swap()?;
    let latest = swap.orders().last()?;
    if swap.order_terms(latest).sell_token() != plan.sell_token()
        || swap.order_terms(latest).buy_token() != plan.buy_token()
        || latest.bounds().spend_amount() != plan.amount()
        || swap.proof().inputs().len() != plan.input_count()
    {
        return None;
    }
    let inputs = swap
        .proof()
        .inputs()
        .iter()
        .map(|input| utxos.iter().find(|utxo| input.matches(utxo)).cloned())
        .collect::<Option<Vec<_>>>()?;
    if !inputs.iter().all(|input| {
        plan.size
            .preview
            .selected_inputs
            .contains(&SelectedInputIdentity::from_utxo(input))
    }) {
        return None;
    }
    let pre_hook = record
        .issued()
        .iter()
        .find(|payload| payload.hash() == latest.pre_hook().payload())?;
    let transactions = RelayAdapt7702::executeCall::abi_decode(pre_hook.context().calldata())
        .ok()?
        ._transactions;
    (transactions.len() == plan.transaction_count()
        && keccak256(transactions.abi_encode()) == swap.proof().digest())
    .then_some((transactions, inputs))
}

/// The persisted reservation must cover exactly the notes whose nullifiers the pre-hook spends.
fn require_input_nullifiers(
    transactions: &[Transaction],
    mut supplied: Vec<(u32, B256)>,
) -> Result<()> {
    let mut expected = transactions
        .iter()
        .flat_map(|transaction| {
            transaction
                .nullifiers
                .iter()
                .map(|nullifier| (u32::from(transaction.boundParams.treeNumber), *nullifier))
        })
        .collect::<Vec<_>>();
    expected.sort_unstable();
    supplied.sort_unstable();
    if expected != supplied || supplied.is_empty() {
        return Err(eyre!(
            "swap input reservations do not match the proved pre-hook"
        ));
    }
    Ok(())
}

/// Pre-hook actions in order: the deadline guard, the invalidation of a still-live earlier
/// order, and the exact approval of the sell amount to the vault relayer.
fn pre_hook_calls(
    profile: &SwapProfile,
    executor: Address,
    sell_token: Address,
    amount: U256,
    valid_to: u32,
    invalidates: Option<OrderUid>,
) -> Result<Vec<Call>> {
    let deadline = ExecutorAction::Deadline {
        target: profile.deadline_guard(),
        deadline: u64::from(valid_to),
    };
    let invalidate = invalidates.map(|order_uid| ExecutorAction::InvalidateOrder {
        settlement: profile.settlement(),
        order_uid,
    });
    let approve = ExecutorAction::Approve {
        token: sell_token,
        spender: profile.vault_relayer(),
        amount,
    };
    [Some(deadline), invalidate, Some(approve)]
        .into_iter()
        .flatten()
        .map(|action| Ok(action.call(executor)?))
        .collect()
}

/// What the order sells after the pre-hook unshields `amount`. Railgun takes its unshield fee
/// from the unshielded value, `amount * fee_bps / 10_000` rounded down, so the executor holds
/// exactly the rest.
fn order_sell_amount(amount: U256, unshield_fee_bps: U256) -> Result<U256> {
    if unshield_fee_bps >= FEE_BASIS_POINTS_DENOMINATOR {
        return Err(eyre!("the Railgun unshield fee is out of range"));
    }
    Ok(amount - railgun_protocol_fee_amount(amount, unshield_fee_bps))
}

/// One executor unshield of the private spend to the executor.
fn pre_hook_request(
    context: ExecutorContext,
    sell_token: Address,
    amount: U256,
    calls: Vec<Call>,
) -> MixedPrivateActionRequest {
    MixedPrivateActionRequest {
        executor: Some(context),
        executor_calls: calls,
        private_sends: Vec::new(),
        public_unshields: vec![CompositeUnshieldLeg {
            token_address: sell_token,
            amount,
            recipient: CompositeUnshieldRecipient::RelayAdapt,
            role: CompositeUnshieldLegRole::Primary,
        }],
        relay_actions: None,
        // The solver's settlement sets the gas price; no broadcaster fee binds a minimum.
        min_gas_price: 0,
        verify_proof: false,
        spend_up_to: false,
        rebuild: None,
    }
}

/// The post-hook calls of `delivery`, with placeholders for the terms only known at signing.
/// Every such argument is a static ABI word, so the calls encode to the signed calls' length.
fn placeholder_post_hook_calls(
    executor: Address,
    buy_token: Address,
    delivery: SwapDelivery,
) -> Result<Vec<Call>> {
    Ok(match delivery {
        SwapDelivery::Bridge(
            bridge @ BridgeDelivery {
                provider: BridgeProvider::Across,
                ..
            },
        ) => bridge_deposit_calls(
            executor,
            Address::ZERO,
            SpokePool::depositV3Call {
                depositor: executor,
                recipient: bridge.receiver,
                inputToken: buy_token,
                outputToken: bridge.destination_token,
                inputAmount: U256::ONE,
                outputAmount: U256::ONE,
                destinationChainId: U256::from(bridge.destination_chain),
                exclusiveRelayer: Address::ZERO,
                quoteTimestamp: u32::MAX,
                fillDeadline: u32::MAX,
                exclusivityParameter: u32::MAX,
                message: Bytes::new(),
            },
            matches!(bridge.surplus, BridgeSurplus::Reshield)
                .then_some(placeholder_shield(buy_token)),
        )?,
        _ => guarded_shield_calls(
            executor,
            buy_token,
            U256::ONE,
            placeholder_shield(buy_token),
        )?,
    })
}

/// Shield requests are static ABI types, so this encodes to the signed request's length.
const fn placeholder_shield(token: Address) -> ShieldRequest {
    ShieldRequest {
        preimage: CommitmentPreimage {
            npk: B256::ZERO,
            token: TokenData::erc20(token),
            value: U120::ZERO,
        },
        ciphertext: ShieldCiphertext {
            encryptedBundle: [B256::ZERO; 3],
            shieldKey: B256::ZERO,
        },
    }
}

/// App data with the executor as the target of its hooks. `post_hook` holds the post-hook's
/// calldata and gas limit; without it the post list is empty.
fn swap_app_data(
    app_code: &str,
    executor: Address,
    pre_hook: Bytes,
    pre_hook_gas_limit: u64,
    post_hook: Option<(Bytes, u64)>,
) -> Result<EncodedAppData> {
    Ok(AppData::hooks(
        app_code.to_owned(),
        vec![AppDataHook {
            call_data: pre_hook,
            gas_limit: pre_hook_gas_limit,
            target: executor,
        }],
        post_hook
            .into_iter()
            .map(|(call_data, gas_limit)| AppDataHook {
                call_data,
                gas_limit,
                target: executor,
            })
            .collect(),
    )
    .encode()?)
}

/// A fill-or-kill sell order owned by the executor that pays `receiver`, from
/// [`order_receiver`]. A native `buy_token`, `Address::ZERO`, becomes `GPv2`'s native buy address.
fn swap_order(
    sell_token: Address,
    buy_token: Address,
    receiver: Address,
    sell_amount: U256,
    buy_amount: U256,
    valid_to: u32,
    app_data: B256,
) -> Order {
    Order {
        sellToken: sell_token,
        buyToken: cow_buy_token(buy_token),
        receiver,
        sellAmount: sell_amount,
        buyAmount: buy_amount,
        validTo: valid_to,
        appData: app_data,
        feeAmount: U256::ZERO,
        kind: ORDER_KIND_SELL.to_owned(),
        partiallyFillable: false,
        sellTokenBalance: TOKEN_BALANCE_ERC20.to_owned(),
        buyTokenBalance: TOKEN_BALANCE_ERC20.to_owned(),
    }
}

/// Who an order pays: the executor, whose post-hook reshields or bridges the bought token, an
/// External receiver, or the NEAR Intents deposit address in the order's `bridge` terms.
fn order_receiver(
    executor: Address,
    delivery: SwapDelivery,
    bridge: Option<&BridgeOrderTerms>,
) -> Result<Address> {
    match delivery {
        SwapDelivery::Reshield
        | SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            ..
        }) => Ok(executor),
        SwapDelivery::External { receiver } => Ok(receiver),
        SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::NearIntents,
            ..
        }) => match bridge {
            Some(BridgeOrderTerms::NearIntents(terms)) => Ok(terms.deposit_address),
            _ => Err(eyre!(
                "NEAR Intents orders need their verified deposit address"
            )),
        },
    }
}

/// `CoW`'s buy token for the wallet's `buy_token`: the native marker `Address::ZERO` becomes
/// `GPv2`'s native buy address. Settlement `Trade` events report this token.
pub(super) fn cow_buy_token(buy_token: Address) -> Address {
    if buy_token == Address::ZERO {
        BUY_NATIVE_TOKEN
    } else {
        buy_token
    }
}

/// The token whose anchors price `token`. The native asset uses the chain's wrapped-native
/// anchors, which have the same price.
pub(super) fn anchor_token(chain_id: u64, token: Address) -> Address {
    if token == Address::ZERO {
        crate::amounts::wrapped_native_token_for_chain(chain_id).unwrap_or(token)
    } else {
        token
    }
}

/// The hook-free quote request for `plan`. It names the executor as receiver for every delivery
/// kind: `CoW` prices a sell order the same for any receiver, and an External receiver then
/// leaves the wallet only with the approved order.
fn swap_quote_request(
    plan: &SwapInputPlan,
    sell_amount: U256,
    valid_to: u32,
) -> CowSellQuoteRequest {
    CowSellQuoteRequest {
        sell_token: plan.sell_token,
        buy_token: cow_buy_token(plan.buy_token),
        from: plan.executor(),
        receiver: plan.executor(),
        sell_amount_before_fee: sell_amount,
        valid_to,
    }
}

fn valid_to_after(now: SystemTime, window: Duration) -> Result<u32> {
    now.checked_add(window)
        .and_then(|valid_to| valid_to.duration_since(UNIX_EPOCH).ok())
        .and_then(|valid_to| u32::try_from(valid_to.as_secs()).ok())
        .ok_or_else(|| eyre!("the order's validity is out of range"))
}

/// A missing price is acceptable only when the review already required acknowledgement.
/// An available cached price always enforces the downside limit, even for an unverified review.
fn recheck_swap_price(
    quote: &CowQuoteParameters,
    reviewed_price: &SwapPrice,
    anchor: Option<PairAnchorRate>,
    deviation_bps: u32,
) -> Result<SwapRecheck> {
    Ok(match anchor {
        Some(rate) => {
            match check_quote_against_anchor(
                quote.sell_amount,
                quote.buy_amount,
                rate,
                deviation_bps,
            ) {
                Ok(()) => SwapRecheck::Current(Vec::new()),
                Err(QuoteDeviationError::ExceedsThreshold) => {
                    SwapRecheck::Changed(SwapReviewChange::QuoteDeviates)
                }
                Err(error) => return Err(error.into()),
            }
        }
        None => {
            if *reviewed_price == SwapPrice::Unverified {
                SwapRecheck::Current(Vec::new())
            } else {
                SwapRecheck::Changed(SwapReviewChange::PriceUnavailable)
            }
        }
    })
}

#[cfg(test)]
mod tests;
