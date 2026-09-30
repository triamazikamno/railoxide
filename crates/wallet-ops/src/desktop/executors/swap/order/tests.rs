use alloy::eips::BlockNumHash;
use alloy::primitives::{address, uint};
use railgun_wallet::tx::GasEstimateMode;
use railgun_wallet::{Note, UtxoCommitmentKind, UtxoSource};

use super::super::bridge::{across_bridge_quote, near_bridge_quote};
use super::*;
use crate::bridge::{AcrossFeeQuote, BridgeDestination, NearAssets, OneClickDryQuote};
use crate::vault::ExecutorNonceObservation;

const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const USDC: Address = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");

fn note(tree: u32, position: u64, value: u64) -> Utxo {
    let mut random = [0; 16];
    random[..4].copy_from_slice(&tree.to_be_bytes());
    random[8..].copy_from_slice(&position.to_be_bytes());
    Utxo::new(
        Note::new_change(U256::ONE, WETH, U256::from(value), random),
        tree,
        position,
        UtxoSource {
            tx_hash: B256::repeat_byte(6),
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    )
}

fn mainnet_profile() -> SwapProfile {
    crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
        .unwrap()
        .get(1)
        .unwrap()
        .swap_profile()
        .unwrap()
}

fn builder() -> TransactionBuilder {
    TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: Address::repeat_byte(4),
        relay_adapt_contract: Address::repeat_byte(5),
    }
}

fn delegated_executor() -> DelegatedSwapExecutor {
    DelegatedSwapExecutor {
        operation: ExecutorOperationId::random().unwrap(),
        executor: Address::repeat_byte(0xe0),
        delegate: Address::repeat_byte(0xde),
        setup_payload: B256::repeat_byte(3),
        observed: ExecutorNonceObservation::new(
            BlockNumHash::new(12, B256::repeat_byte(12)),
            U256::ONE,
        ),
    }
}

#[test]
fn swap_planning_offers_the_largest_amount_that_fits_one_order() {
    let profile = mainnet_profile();
    let builder = builder();
    let delegated = delegated_executor();
    let budget = profile.app_data_byte_budget();
    let plan = |utxos: &[Utxo], amount: U256| {
        plan_swap_inputs(
            &builder,
            &profile,
            delegated,
            utxos,
            &SwapAmountRequest {
                sell_token: WETH,
                buy_token: USDC,
                amount,
                delivery: SwapDelivery::Reshield,
                byte_budget: None,
            },
            budget,
            None,
        )
        .unwrap()
    };

    // Too many notes: sixty notes of one tree need five transactions.
    let many = (0..60)
        .map(|position| note(0, position, 1))
        .collect::<Vec<_>>();
    // Spread across trees: each of nine trees needs its own transaction, one more than a batch.
    let spread = (0..9).map(|tree| note(tree, 0, 10)).collect::<Vec<_>>();
    for (utxos, entered) in [(&many, 60_u64), (&spread, 90)] {
        let SwapAmountPlan::TooLarge { largest } = plan(utxos.as_slice(), U256::from(entered))
        else {
            panic!("{entered} does not fit one order");
        };
        assert!(largest.amount() < U256::from(entered));
        assert!(largest.app_data_len() <= budget);
        // The offer is a plan in its own right, the only kind a pre-hook proof is built from.
        assert_eq!(
            plan(utxos.as_slice(), largest.amount()),
            SwapAmountPlan::Fits(largest)
        );
    }
}

// An External order pays its receiver directly: no post-hook in its gas, limits or app data,
// no shield fee in its buy amount, and its quote still names only the executor.
#[test]
fn external_delivery_prices_only_the_pre_hook_and_no_shield_fee() {
    let profile = mainnet_profile();
    let delegated = delegated_executor();
    let receiver = Address::repeat_byte(0x99);
    let notes = [note(0, 0, 1_000_000)];
    let plan = |buy_token, delivery| {
        let SwapAmountPlan::Fits(plan) = plan_swap_inputs(
            &builder(),
            &profile,
            delegated,
            &notes,
            &SwapAmountRequest {
                sell_token: WETH,
                buy_token,
                amount: U256::from(1_000_000),
                delivery,
                byte_budget: None,
            },
            profile.app_data_byte_budget(),
            None,
        )
        .unwrap() else {
            panic!("one note fits one order");
        };
        plan
    };
    let reshield = plan(USDC, SwapDelivery::Reshield);
    let external = plan(Address::ZERO, SwapDelivery::External { receiver });
    assert_eq!(external.post_hook_gas_limit(), None);
    assert_eq!(
        external.hook_gas_estimate(),
        reshield.hook_gas_estimate()
            - post_hook_gas(reshield.gas_model, GasEstimateMode::UpperBound)
    );
    assert!(external.app_data_len() < reshield.app_data_len());

    // 10,002 quoted less a 1-unit hook cost and 1 bp slippage delivers 9,999.
    let quote: CowQuote = serde_json::from_value(serde_json::json!({
        "quote": {
            "sellToken": WETH, "buyToken": USDC,
            "sellAmount": "997500", "buyAmount": "10002",
            "validTo": 1, "feeAmount": "0", "gasAmount": "0", "gasPrice": "0",
            "sellTokenPrice": "1", "kind": "sell", "partiallyFillable": false
        },
        "expiration": "", "id": 7, "verified": true
    }))
    .unwrap();
    let price = |plan: &SwapInputPlan| {
        price_swap_review(
            plan.clone(),
            quote.clone(),
            SwapPrice::Verified {
                rate: PairAnchorRate {
                    sell_rate: U256::ONE,
                    buy_rate: U256::ONE,
                },
                observations: Vec::new(),
            },
            U256::from(25),
            U256::from(25),
            1,
            1,
            U256::ZERO,
            OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
        )
        .unwrap()
    };
    let (reshield_review, external_review) = (price(&reshield), price(&external));
    assert_eq!(external_review.shield_fee_bps(), U256::ZERO);
    assert_eq!(
        external_review.suggested_private_minimum(),
        U256::from(9_999)
    );
    assert_eq!(
        reshield_review.suggested_private_minimum(),
        U256::from(9_975)
    );
    // An approved minimum of 9,975 is the External buy amount; Reshield grosses it up.
    let minimum = U256::from(9_975);
    assert_eq!(external_review.buy_amount_for(minimum).unwrap(), minimum);
    assert_eq!(
        reshield_review.buy_amount_for(minimum).unwrap(),
        U256::from(9_999)
    );

    let request = swap_quote_request(&external, U256::from(997_500), 1);
    assert_eq!(
        (request.from, request.receiver, request.buy_token),
        (delegated.executor, delegated.executor, BUY_NATIVE_TOKEN)
    );
    // The native asset is anchored by the chain's wrapped-native token.
    assert_eq!(anchor_token(1, Address::ZERO), WETH);
}

#[test]
fn orderbook_app_data_rejection_returns_to_a_smaller_offer() {
    let uid = OrderUid::new(B256::repeat_byte(1), Address::repeat_byte(2), 3);
    assert_eq!(
        swap_submission_outcome(Err(CowApiError::AppDataTooLarge), uid, 14_000, 14_336).unwrap(),
        SwapOrderOutcome::Replan {
            byte_budget: 13_999,
            attempt_recorded: true,
        }
    );
    assert!(swap_submission_outcome(Err(CowApiError::NoLiquidity), uid, 14_000, 14_336).is_err());
}

#[test]
fn cached_swap_prices_cannot_silently_drop_or_bypass_verification() {
    let rate = PairAnchorRate {
        sell_rate: U256::from(997_500),
        buy_rate: U256::from(2_000_000),
    };
    let quote: CowQuote = serde_json::from_value(serde_json::json!({
        "quote": {
            "sellToken": WETH, "buyToken": USDC,
            "sellAmount": "997500", "buyAmount": "2000000",
            "validTo": 1, "feeAmount": "0", "gasAmount": "0", "gasPrice": "0",
            "sellTokenPrice": "1000000000000", "kind": "sell", "partiallyFillable": false
        },
        "expiration": "", "id": 7, "verified": true
    }))
    .unwrap();
    let verified = SwapPrice::Verified {
        rate,
        observations: Vec::new(),
    };
    assert!(matches!(
        recheck_swap_price(&quote.quote, &verified, None, 300).unwrap(),
        SwapRecheck::Changed(SwapReviewChange::PriceUnavailable)
    ));
    assert!(matches!(
        recheck_swap_price(&quote.quote, &SwapPrice::Unverified, None, 300).unwrap(),
        SwapRecheck::Current(observations) if observations.is_empty()
    ));
    let cached = |buy_rate| Some(PairAnchorRate { buy_rate, ..rate });
    assert!(matches!(
        recheck_swap_price(
            &quote.quote,
            &SwapPrice::Unverified,
            cached(rate.buy_rate),
            300
        )
        .unwrap(),
        SwapRecheck::Current(_)
    ));
    assert!(matches!(
        recheck_swap_price(
            &quote.quote,
            &SwapPrice::Unverified,
            cached(rate.buy_rate * U256::from(2)),
            300
        )
        .unwrap(),
        SwapRecheck::Changed(SwapReviewChange::QuoteDeviates)
    ));
}

fn near_delivery() -> BridgeDelivery {
    BridgeDelivery {
        provider: BridgeProvider::NearIntents,
        destination_chain: 56,
        receiver: Address::repeat_byte(0x99),
        destination_token: Address::repeat_byte(0x94),
        surplus: BridgeSurplus::BridgedByProvider,
    }
}

/// A plan selling 1,000,000 WETH base units for USDC from one note.
fn plan_for(delivery: SwapDelivery) -> SwapInputPlan {
    let profile = mainnet_profile();
    let SwapAmountPlan::Fits(plan) = plan_swap_inputs(
        &builder(),
        &profile,
        delegated_executor(),
        &[note(0, 0, 1_000_000)],
        &SwapAmountRequest {
            sell_token: WETH,
            buy_token: USDC,
            amount: U256::from(1_000_000),
            delivery,
            byte_budget: None,
        },
        profile.app_data_byte_budget(),
        None,
    )
    .unwrap() else {
        panic!("one note fits one order");
    };
    plan
}

/// A NEAR Intents review whose `CoW` price is verified, without its bridge quote.
fn bridge_review() -> SwapReview {
    let quote: CowQuote = serde_json::from_value(serde_json::json!({
        "quote": {
            "sellToken": WETH, "buyToken": USDC,
            "sellAmount": "997500", "buyAmount": "10002",
            "validTo": 1, "feeAmount": "0", "gasAmount": "0", "gasPrice": "0",
            "sellTokenPrice": "1", "kind": "sell", "partiallyFillable": false
        },
        "expiration": "", "id": 7, "verified": true
    }))
    .unwrap();
    price_swap_review(
        plan_for(SwapDelivery::Bridge(near_delivery())),
        quote,
        SwapPrice::Verified {
            rate: PairAnchorRate {
                sell_rate: U256::ONE,
                buy_rate: U256::ONE,
            },
            observations: Vec::new(),
        },
        U256::from(25),
        U256::from(25),
        1,
        1,
        U256::ZERO,
        OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
    )
    .unwrap()
}

/// 1,000 USDC bought on the swap's chain for the bridge.
const BRIDGED: U256 = uint!(1_000_000_000_U256);

fn near_destination(same_asset: bool, destination_decimals: u8) -> BridgeDestination {
    BridgeDestination {
        destination_token: Address::ZERO,
        intermediate: USDC,
        symbol: "BNB".into(),
        same_asset,
        near: Some(NearAssets {
            origin_asset: "eth:usdc".into(),
            destination_asset: "bsc:bnb".into(),
            origin_decimals: 6,
            destination_decimals,
        }),
    }
}

fn dry_quote(amount_out: U256, min_amount_out: U256) -> OneClickDryQuote {
    OneClickDryQuote {
        amount_in: BRIDGED,
        min_amount_in: BRIDGED,
        amount_out,
        min_amount_out,
        time_estimate_sec: Some(20),
    }
}

/// USDC on Ethereum at 3,000 USDC and 3,000 USD per ETH, to BNB at 600 USD per BNB, as
/// `TokenAnchorRateCache::cached_bridge_leg_rate` scales them: 1,000 USDC is worth 1.67 BNB.
const USDC_TO_BNB: PairAnchorRate = PairAnchorRate {
    sell_rate: uint!(1_800_000_000_000_000_000_U256),
    buy_rate: uint!(3_000_000_000_000_000_000_000_000_000_U256),
};

// Across's output for the buy amount is the destination minimum, and its whole relay fee,
// which includes the LP fee, is the bridge fee the high-cost check counts: 15% here. The
// deposit contract comes from the profile, never from the API.
#[test]
fn across_quote_prices_the_relay_fee_and_requires_the_profiles_spoke_pool() {
    let spoke_pool = Address::repeat_byte(0x5b);
    let fees = AcrossFeeQuote {
        output_amount: U256::from(850_000_000),
        total_relay_fee_total: U256::from(150_000_000),
        lp_fee_total: U256::from(50_000_000),
        timestamp: 1,
        fill_deadline: 2,
        exclusive_relayer: Address::ZERO,
        exclusivity_deadline: 0,
        spoke_pool,
        destination_spoke_pool: Address::repeat_byte(0x5c),
        is_amount_too_low: false,
        min_deposit: U256::ONE,
        max_deposit: U256::MAX,
        estimated_fill_time_sec: 12,
    };
    let quote = across_bridge_quote(&fees, spoke_pool).unwrap();
    assert_eq!(
        (quote.destination_minimum, quote.fee, quote.leg),
        (
            U256::from(850_000_000),
            Some(U256::from(150_000_000)),
            BridgeLegPrice::SameAsset
        )
    );
    assert!(across_bridge_quote(&fees, Address::repeat_byte(0x5d)).is_err());
}

// An Across post-hook guards, approves the SpokePool and deposits, and with Reshield also
// shields the surplus. The plan prices those calls and sizes the app data for them.
#[test]
fn across_plans_price_and_size_the_deposit_post_hook() {
    let across = |surplus| {
        plan_for(SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            surplus,
            ..near_delivery()
        }))
    };
    let (keep, reshielding) = (
        across(BridgeSurplus::KeepInAccount),
        across(BridgeSurplus::Reshield),
    );
    let (reshield, near) = (
        plan_for(SwapDelivery::Reshield),
        plan_for(SwapDelivery::Bridge(near_delivery())),
    );
    assert_eq!(
        reshielding.hook_gas_estimate() - keep.hook_gas_estimate(),
        reshield.gas_model.shield(GasEstimateMode::UpperBound, 1)
    );
    assert!(reshielding.hook_gas_estimate() > reshield.hook_gas_estimate());
    assert!(reshielding.post_hook_gas_limit() > reshield.post_hook_gas_limit());
    assert_eq!(near.post_hook_gas_limit(), None);
    assert!(near.app_data_len() < keep.app_data_len());
    assert!(keep.app_data_len() < reshielding.app_data_len());
    assert!(reshield.app_data_len() < reshielding.app_data_len());
}

// A fixed Across deposit is only part of the user's outcome. Estimate the remaining payout
// after gas, then charge the shield fee only on that remainder. The deposit uses a 25% gas-price
// cushion; the estimate uses the raw RPC price, independently of CoW's price.
#[test]
fn estimated_outcome_deducts_gas_and_shields_only_the_across_surplus() {
    for surplus in [BridgeSurplus::Reshield, BridgeSurplus::KeepInAccount] {
        let plan = plan_for(SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            surplus,
            ..near_delivery()
        }));
        let quote: CowQuote = serde_json::from_value(serde_json::json!({
            "quote": {
                "sellToken": WETH, "buyToken": USDC,
                "sellAmount": "997500", "buyAmount": "10000000",
                "validTo": 1, "feeAmount": "0", "gasAmount": "0", "gasPrice": "99",
                "sellTokenPrice": "1", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }))
        .unwrap();
        let rate = NativeBuyRate::Anchor(uint!(1_000_000_000_000_000_000_U256));
        let bound = price_order_limit(&OrderLimitParams {
            quote: &quote.quote,
            hook_gas: plan.hook_gas_estimate(),
            // 2 wei + 25%, rounded up to a whole wei.
            gas_price_wei: 3,
            hook_data_cost_wei: U256::from(100_000),
            native_rate: rate,
            slippage_bps: 100,
            shield_fee_bps: U256::ZERO,
        })
        .unwrap();
        let expected_hook_cost = U256::from(plan.expected_hook_gas() * 2 + 100_000);
        let review = price_swap_review(
            plan,
            quote,
            SwapPrice::Verified {
                rate: PairAnchorRate {
                    sell_rate: U256::ONE,
                    buy_rate: uint!(1_000_000_000_000_000_000_U256),
                },
                observations: Vec::new(),
            },
            U256::from(25),
            U256::from(25),
            100,
            2,
            U256::from(100_000),
            OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
        )
        .unwrap();
        assert_eq!(
            review.limit, bound,
            "the fixed deposit must use the cushioned RPC price"
        );
        assert_eq!(review.estimated_hook_cost(), expected_hook_cost);
        assert!(review.estimated_hook_cost() < review.hook_cost());
        let bought = U256::from(10_000_000) - review.estimated_hook_cost();
        let gross_surplus = bought - bound.buy_amount;
        let fee = if surplus == BridgeSurplus::Reshield {
            gross_surplus * U256::from(25) / U256::from(10_000)
        } else {
            U256::ZERO
        };
        assert!(gross_surplus > U256::ZERO);
        assert_eq!(review.estimated_source_surplus(), Some(gross_surplus - fee));
        assert_eq!(review.shield_fee_on_output(bought), fee);
        assert_eq!(review.shield_fee_on_output(bound.buy_amount), U256::ZERO);
        // The return and deposit together account for the payout after gas and shield fees.
        assert_eq!(
            review.estimated_source_surplus().unwrap() + bound.buy_amount + fee,
            bought
        );
    }
    assert_eq!(
        bridge_review().estimated_source_surplus(),
        None,
        "NEAR forwards the whole payout and must not show a source refund"
    );
}

#[test]
fn near_quote_prices_its_minimum_through_the_cross_chain_anchor() {
    let destination = near_destination(false, 18);
    let quote = |min_amount_out| {
        near_bridge_quote(
            BRIDGED,
            &dry_quote(uint!(1_660_000_000_000_000_000_U256), min_amount_out),
            Some(USDC_TO_BNB),
            &destination,
            300,
        )
    };
    // A minimum of 1.65 BNB is worth 990 USDC, so the bridge costs 10 USDC.
    let minimum = uint!(1_650_000_000_000_000_000_U256);
    let priced = quote(minimum).unwrap();
    assert_eq!(
        (priced.destination_minimum, priced.fee, priced.leg),
        (
            minimum,
            Some(U256::from(10_000_000)),
            BridgeLegPrice::Verified(USDC_TO_BNB)
        )
    );
    // 1.5 BNB is 10% below the anchor, and fails like a CoW quote beyond the deviation.
    assert_eq!(
        quote(uint!(1_500_000_000_000_000_000_U256))
            .unwrap_err()
            .downcast_ref::<QuoteDeviationError>(),
        Some(&QuoteDeviationError::ExceedsThreshold)
    );
}

// Without anchors, a same-asset leg's fee is its shortfall in the bought token's decimals. A
// leg between different assets has no known fee, and its review needs acknowledgement.
#[test]
fn near_leg_without_anchors_is_same_asset_or_needs_acknowledgement() {
    let same_asset = near_bridge_quote(
        BRIDGED,
        &dry_quote(
            uint!(996_000_000_000_000_000_000_U256),
            uint!(995_000_000_000_000_000_000_U256),
        ),
        None,
        &near_destination(true, 18),
        300,
    )
    .unwrap();
    assert_eq!(
        (same_asset.fee, same_asset.leg),
        (Some(U256::from(5_000_000)), BridgeLegPrice::SameAsset)
    );

    let minimum = uint!(1_650_000_000_000_000_000_U256);
    let unverified = near_bridge_quote(
        BRIDGED,
        &dry_quote(minimum, minimum),
        None,
        &near_destination(false, 18),
        300,
    )
    .unwrap();
    assert_eq!(
        (unverified.fee, unverified.leg),
        (None, BridgeLegPrice::Unverified)
    );
    let mut review = bridge_review();
    review.bridge = Some(unverified);
    assert!(!review.price_verified());
    let private_minimum = review.suggested_private_minimum();
    assert!(review.approval(private_minimum, false).is_err());
    let approval = review.approval(private_minimum, true).unwrap();
    assert_eq!(
        (approval.price_verified, approval.bounds.destination_minimum),
        (Some(false), Some(minimum))
    );
}

#[test]
fn a_lower_destination_minimum_needs_a_new_review() {
    let quote = |minimum: u64| SwapBridgeQuote {
        provider: BridgeProvider::NearIntents,
        destination_minimum: U256::from(minimum),
        expected_output: U256::from(minimum),
        fee: Some(U256::ZERO),
        leg: BridgeLegPrice::SameAsset,
        fill_time_sec: None,
    };
    let mut review = bridge_review();
    review.bridge = Some(quote(1_000));
    let mut approval = review
        .approval(review.suggested_private_minimum(), false)
        .unwrap();
    for (fresh, change) in [
        (
            999,
            Some(SwapReviewChange::DestinationMinimum {
                approved: U256::from(1_000),
                current: U256::from(999),
            }),
        ),
        (1_000, None),
        (1_001, None),
    ] {
        review.bridge = Some(quote(fresh));
        assert_eq!(review.approval_change(&approval), change, "{fresh}");
    }
    // An approval that binds no destination minimum can't sign a Bridge order.
    approval.bounds.destination_minimum = None;
    assert_eq!(
        review.approval_change(&approval),
        Some(SwapReviewChange::DestinationMinimum {
            approved: U256::ZERO,
            current: U256::from(1_001),
        })
    );
}
