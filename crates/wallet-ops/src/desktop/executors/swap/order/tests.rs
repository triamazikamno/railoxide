use alloy::eips::BlockNumHash;
use alloy::primitives::{address, uint};
use alloy::signers::local::PrivateKeySigner;
use railgun_wallet::tx::GasEstimateMode;
use railgun_wallet::{Note, UtxoCommitmentKind, UtxoSource};

use super::super::bridge::{
    BridgeSigning, SwapPrivateBridgeQuote, across_bridge_quote, across_order_terms,
    delivery_allowance_rate, near_bridge_quote,
};
use super::*;
use crate::bridge::{AcrossFeeQuote, BridgeDestination, NearAssets, OneClickDryQuote};
use crate::cow::{
    DeliveryAllowanceRate, GAS_SHARE_BALANCED_BPS, GAS_SHARE_LOOSE_BPS, GAS_SHARE_TIGHT_BPS,
};
use crate::settings::BridgeReceiverRejection;
use crate::vault::{BridgePrivateDelivery, BridgeShieldFailure, ExecutorNonceObservation};

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

    // 10,002 quoted less 1 bp tolerance and a 1-unit gas allowance delivers 9,999.
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
            GAS_SHARE_BALANCED_BPS,
            Duration::from_mins(10),
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
        private: None,
    }
}

/// An Across delivery that shields to the wallet from the destination stealth `account`.
fn private_delivery(account: Address, on_shield_failure: BridgeShieldFailure) -> BridgeDelivery {
    BridgeDelivery {
        provider: BridgeProvider::Across,
        receiver: account,
        surplus: BridgeSurplus::KeepInAccount,
        private: Some(BridgePrivateDelivery { on_shield_failure }),
        ..near_delivery()
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
    bridge_review_for(near_delivery())
}

/// A review of `delivery` whose `CoW` price is verified, without its bridge quote.
fn bridge_review_for(delivery: BridgeDelivery) -> SwapReview {
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
        plan_for(SwapDelivery::Bridge(delivery)),
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
        GAS_SHARE_BALANCED_BPS,
        Duration::from_mins(10),
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
// deposit contract comes from the profile, never from the API. A private delivery's minimum is
// that output less the delivery allowance, and what reaches the private balance also pays the
// destination chain's shield fee.
#[test]
fn across_quote_prices_the_relay_fee_and_requires_the_profiles_spoke_pool() {
    let spoke_pool = Address::repeat_byte(0x5b);
    let fees = AcrossFeeQuote {
        output_amount: U256::from(850_000_000),
        total_relay_fee_total: U256::from(150_000_000),
        total_relay_fee_pct: U256::ZERO,
        relayer_gas_fee_total: U256::ZERO,
        // 0.1% of the input.
        relayer_gas_fee_pct: uint!(1_000_000_000_000_000_U256),
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
    let quote = across_bridge_quote(&fees, spoke_pool, None).unwrap();
    assert_eq!(
        (quote.destination_minimum, quote.fee, quote.leg),
        (
            U256::from(850_000_000),
            Some(U256::from(150_000_000)),
            BridgeLegPrice::SameAsset
        )
    );
    assert_eq!(
        (quote.private, quote.received_minimum()),
        (None, quote.destination_minimum)
    );
    assert!(across_bridge_quote(&fees, Address::repeat_byte(0x5d), None).is_err());

    // An allowance of 50 leaves 800 for the destination account, whose shield takes 25 bp, 2.
    // The relay fee stays the bridge fee.
    let allowance = U256::from(50_000_000);
    let private = across_bridge_quote(&fees, spoke_pool, Some(allowance)).unwrap();
    assert_eq!(
        (
            private.destination_minimum,
            private.expected_output,
            private.fee
        ),
        (
            U256::from(800_000_000),
            U256::from(800_000_000),
            Some(U256::from(150_000_000))
        )
    );
    assert_eq!(
        private.private,
        Some(SwapPrivateBridgeQuote {
            quoted_output: fees.output_amount,
            delivery_allowance: allowance,
            destination_shield_fee_bps: RAILGUN_PROTOCOL_FEE_BPS,
        })
    );
    assert_eq!(private.received_minimum(), U256::from(798_000_000));
    assert_eq!(
        across_bridge_quote(&fees, spoke_pool, Some(fees.output_amount))
            .unwrap_err()
            .downcast_ref::<OrderLimitError>(),
        Some(&OrderLimitError::DeliveryAllowanceExceedsOutput {
            allowance: fees.output_amount,
            output: fees.output_amount,
        })
    );

    // The allowance converts at par for the destination chain's wrapped native token, by the
    // cached anchor for another token, and without one from the quote's own gas fee: 0.1% of
    // the output here, since the whole fee percentage is zero.
    let delivery = private_delivery(Address::repeat_byte(0xeb), BridgeShieldFailure::default());
    let (token, anchor) = (delivery.destination_token, U256::from(600_000_000));
    for (wrapped_native, anchor, rate) in [
        (Some(token), Some(anchor), DeliveryAllowanceRate::Par),
        (
            Some(WETH),
            Some(anchor),
            DeliveryAllowanceRate::Anchor(anchor),
        ),
        (
            Some(WETH),
            None,
            DeliveryAllowanceRate::QuoteScaled {
                relayer_gas_fee: U256::from(850_000),
            },
        ),
    ] {
        assert_eq!(
            delivery_allowance_rate(delivery, wrapped_native, anchor, &fees).unwrap(),
            rate
        );
    }
    // A quote whose fee takes the whole input has no gas fee to scale.
    let unusable = AcrossFeeQuote {
        total_relay_fee_pct: uint!(1_000_000_000_000_000_000_U256),
        ..fees
    };
    assert!(delivery_allowance_rate(delivery, Some(WETH), None, &unusable).is_err());
}

// The destination SpokePool a quote names is never trusted as a pool. While signing, it only
// refuses a public receiver equal to it, whose proceeds would be stranded there.
#[test]
fn across_terms_refuse_a_public_receiver_that_is_the_quoted_destination_pool() {
    let spoke_pool = Address::repeat_byte(0x5b);
    let destination_spoke_pool = Address::repeat_byte(0x5c);
    let fees = AcrossFeeQuote {
        output_amount: U256::from(850_000_000),
        total_relay_fee_total: U256::from(150_000_000),
        total_relay_fee_pct: U256::ZERO,
        relayer_gas_fee_total: U256::ZERO,
        relayer_gas_fee_pct: U256::ZERO,
        lp_fee_total: U256::ZERO,
        timestamp: 1_000,
        fill_deadline: 4_000,
        exclusive_relayer: Address::ZERO,
        exclusivity_deadline: 0,
        spoke_pool,
        destination_spoke_pool,
        is_amount_too_low: false,
        min_deposit: U256::ONE,
        max_deposit: U256::MAX,
        estimated_fill_time_sec: 12,
    };
    let terms = |receiver| {
        across_order_terms(
            &fees,
            spoke_pool,
            USDC,
            BridgeDelivery {
                provider: BridgeProvider::Across,
                receiver,
                ..near_delivery()
            },
            U256::from(1_000_000_000),
            fees.output_amount,
            2_000,
            None,
        )
    };
    assert!(matches!(
        terms(Address::repeat_byte(0x99)),
        Ok(BridgeSigning::Terms(_))
    ));
    assert_eq!(
        terms(destination_spoke_pool)
            .err()
            .and_then(|error| error.downcast_ref::<BridgeReceiverRejection>().copied()),
        Some(BridgeReceiverRejection::SpokePool)
    );
}

// The plan sizes a private delivery's post-hook with placeholders. It must encode to the length
// of the post-hook signed later, whose deposit carries the handler message around the
// destination account's signed shield, with or without a fallback recipient.
#[test]
fn private_delivery_placeholder_has_the_signed_post_hooks_length() {
    let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(7)).unwrap();
    let (executor, account) = (Address::repeat_byte(0xe0), signer.address());
    let (handler, spoke_pool) = (Address::repeat_byte(0x7e), Address::repeat_byte(0x5b));
    let token = near_delivery().destination_token;
    let amount = U256::from(123_456_789);
    let calls = guarded_shield_calls(
        account,
        token,
        amount,
        ShieldRequest {
            preimage: CommitmentPreimage {
                npk: B256::repeat_byte(1),
                token: TokenData::erc20(token),
                value: U120::ZERO,
            },
            ciphertext: ShieldCiphertext {
                encryptedBundle: [B256::repeat_byte(2); 3],
                shieldKey: B256::repeat_byte(3),
            },
        },
    )
    .unwrap();
    let nonce = U256::from(7);
    let hash = post_hook_signing_hash(&calls, nonce, 56, account);
    let shield_multicall = signed_post_hook_calldata(
        calls,
        nonce,
        56,
        account,
        &signer.sign_hash_sync(&hash).unwrap(),
    )
    .unwrap();
    for on_shield_failure in [
        BridgeShieldFailure::RefundOnOrigin,
        BridgeShieldFailure::KeepOnDestination,
    ] {
        let delivery = private_delivery(account, on_shield_failure);
        let signed = private_bridge_deposit_calls(
            executor,
            spoke_pool,
            SpokePool::depositV3Call {
                depositor: executor,
                recipient: handler,
                inputToken: USDC,
                outputToken: token,
                inputAmount: amount,
                outputAmount: amount,
                destinationChainId: U256::from(delivery.destination_chain),
                exclusiveRelayer: Address::repeat_byte(0x77),
                quoteTimestamp: 1,
                fillDeadline: 2,
                exclusivityParameter: 3,
                message: Bytes::new(),
            },
            AcrossPrivateDelivery {
                handler,
                destination_executor: account,
                shield_multicall: shield_multicall.clone(),
                fallback: (on_shield_failure == BridgeShieldFailure::KeepOnDestination)
                    .then_some(account),
            },
            None,
        )
        .unwrap();
        let placeholder =
            placeholder_post_hook_calls(executor, USDC, SwapDelivery::Bridge(delivery)).unwrap();
        assert_eq!(
            placeholder.abi_encode().len(),
            signed.abi_encode().len(),
            "{on_shield_failure:?}"
        );
        // The destination chain's data cost is priced for the same message length.
        let message = SpokePool::depositV3Call::abi_decode(&signed[2].data)
            .unwrap()
            .message;
        assert_eq!(
            placeholder_private_delivery_message_len(delivery).unwrap(),
            message.len(),
            "{on_shield_failure:?}"
        );
    }
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

// One pricing pass sets the minimum from the gas share for every delivery: the quote's swap gas
// and the hooks' gas at the RPC price with its 25% cushion, independently of CoW's price, and
// the shield fee only for Private delivery. A fixed Across deposit is the order's buy amount,
// and the source-chain return is bounded by the best case, less the shield fee on the surplus.
#[test]
fn review_prices_the_gas_share_once_and_reprices_without_requests() {
    // A network fee of 2,500 sell units is worth 25,062 buy units at the quoted rate.
    let quote: CowQuote = serde_json::from_value(serde_json::json!({
        "quote": {
            "sellToken": WETH, "buyToken": USDC,
            "sellAmount": "997500", "buyAmount": "10000000",
            "validTo": 1, "feeAmount": "2500", "gasAmount": "100000", "gasPrice": "99",
            "sellTokenPrice": "1", "kind": "sell", "partiallyFillable": false
        },
        "expiration": "", "id": 7, "verified": true
    }))
    .unwrap();
    let best_case = U256::from(10_025_062);
    let across = |surplus| {
        SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            surplus,
            ..near_delivery()
        })
    };
    let price_at = |plan: &SwapInputPlan, gas_share_bps, gas_price_wei| {
        price_swap_review(
            plan.clone(),
            quote.clone(),
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
            gas_share_bps,
            Duration::from_mins(30),
            gas_price_wei,
            U256::from(100_000),
            OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
        )
    };
    let price = |plan: &SwapInputPlan, gas_share_bps| price_at(plan, gas_share_bps, 1).unwrap();
    for delivery in [
        SwapDelivery::Reshield,
        SwapDelivery::External {
            receiver: Address::repeat_byte(0x77),
        },
        across(BridgeSurplus::Reshield),
        across(BridgeSurplus::KeepInAccount),
    ] {
        let plan = plan_for(delivery);
        let review = price(&plan, GAS_SHARE_BALANCED_BPS);
        // 1 wei + 25%, rounded up to a whole wei, for swap and hook gas, plus the data cost.
        let gas_estimate = U256::from((100_000 + plan.hook_gas_estimate()) * 2 + 100_000);
        assert_eq!(
            (
                review.best_case(),
                review.gas_estimate(),
                review.gas_allowance()
            ),
            (
                best_case,
                gas_estimate,
                gas_estimate.div_ceil(U256::from(4))
            ),
            "{delivery:?}"
        );
        let pre_fee = best_case * U256::from(9_900) / U256::from(10_000) - review.gas_allowance();
        let shield_fee_bps = match delivery {
            SwapDelivery::Reshield => U256::from(25),
            _ => U256::ZERO,
        };
        let minimum = pre_fee - pre_fee * shield_fee_bps / U256::from(10_000);
        assert_eq!(review.suggested_private_minimum(), minimum, "{delivery:?}");
        assert_eq!(review.valid_for(), Duration::from_mins(30));

        if let SwapDelivery::Bridge(BridgeDelivery { surplus, .. }) = delivery {
            // The Across deposit is the order's buy amount, which is the minimum.
            assert_eq!(review.limit.buy_amount, minimum);
            let gross_surplus = best_case - minimum;
            let fee = if surplus == BridgeSurplus::Reshield {
                gross_surplus * U256::from(25) / U256::from(10_000)
            } else {
                U256::ZERO
            };
            assert_eq!(review.estimated_source_surplus(), Some(gross_surplus - fee));
            assert_eq!(review.shield_fee_on_output(best_case), fee);
            assert_eq!(review.shield_fee_on_output(minimum), U256::ZERO);
        } else {
            assert_eq!(review.estimated_source_surplus(), None);
        }

        // Another share reprices the same quote and gas inputs, and needs no request: the
        // result equals a fresh review at that share. A bridge leg quoted for the old amount
        // is dropped.
        let mut bridged = review.clone();
        bridged.bridge = Some(SwapBridgeQuote {
            provider: BridgeProvider::Across,
            destination_minimum: minimum,
            expected_output: minimum,
            fee: Some(U256::ZERO),
            leg: BridgeLegPrice::SameAsset,
            fill_time_sec: None,
            private: None,
        });
        for share in [0, GAS_SHARE_TIGHT_BPS, GAS_SHARE_LOOSE_BPS] {
            let repriced = bridged.with_gas_share(share).unwrap();
            let fresh = price(&plan, share);
            assert_eq!(repriced.limit, fresh.limit, "{delivery:?} {share}");
            assert_eq!(repriced.gas_share_bps(), share);
            assert!(repriced.bridge().is_none());
        }

        // Another receiver changes only the plan's delivery: the plan equals a fresh one for
        // that receiver, and the limit, the validity and the bridge leg stay as quoted.
        let receiver = Address::repeat_byte(0x78);
        let moved = bridged.with_receiver(receiver);
        let delivered = match delivery {
            SwapDelivery::Reshield => delivery,
            SwapDelivery::External { .. } => SwapDelivery::External { receiver },
            SwapDelivery::Bridge(bridge) => {
                SwapDelivery::Bridge(BridgeDelivery { receiver, ..bridge })
            }
        };
        // Each fresh plan reserves its own operation.
        let mut expected = plan_for(delivered);
        expected.executor = moved.plan.executor;
        assert_eq!(moved.plan, expected, "{delivery:?}");
        assert_eq!(moved.limit, bridged.limit, "{delivery:?}");
        assert_eq!(
            (moved.gas_share_bps(), moved.valid_for(), moved.bridge()),
            (
                bridged.gas_share_bps(),
                bridged.valid_for(),
                bridged.bridge()
            ),
            "{delivery:?}"
        );
    }

    // Gas of about 60 million buy units exhausts Balanced's minimum but leaves Tight's
    // positive, so the review falls back to Tight and reports it. When Tight fails too, pricing
    // fails.
    let plan = plan_for(SwapDelivery::Reshield);
    let units = 100_000 + plan.hook_gas_estimate();
    // A price of 4k wei is cushioned to 5k wei.
    let gas_price_wei = u128::from(4 * (12_000_000 / units));
    let fallback = price_at(&plan, GAS_SHARE_BALANCED_BPS, gas_price_wei).unwrap();
    assert_eq!(fallback.gas_share_bps(), GAS_SHARE_TIGHT_BPS);
    assert_eq!(
        fallback.limit,
        price_at(&plan, GAS_SHARE_TIGHT_BPS, gas_price_wei)
            .unwrap()
            .limit
    );
    assert!(matches!(
        price_at(&plan, GAS_SHARE_BALANCED_BPS, gas_price_wei * 10)
            .unwrap_err()
            .downcast_ref::<OrderLimitError>(),
        Some(OrderLimitError::HookCostExceedsOutput { .. })
    ));
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
        private: None,
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

// A private delivery's approval also binds the destination account, the failure choice and the
// destination shield fee, and records the delivery allowance. Each difference needs a review
// that names it. The approved allowed gas is one unit here, so the cushion is zero.
#[test]
fn a_private_deliverys_destination_terms_need_a_new_review() {
    let account = Address::repeat_byte(0xeb);
    let delivery = private_delivery(account, BridgeShieldFailure::RefundOnOrigin);
    let quote = |output: u64, allowance: u64, shield_fee_bps: u64| SwapBridgeQuote {
        provider: BridgeProvider::Across,
        destination_minimum: U256::from(output - allowance),
        expected_output: U256::from(output - allowance),
        fee: Some(U256::ZERO),
        leg: BridgeLegPrice::SameAsset,
        fill_time_sec: None,
        private: Some(SwapPrivateBridgeQuote {
            quoted_output: U256::from(output),
            delivery_allowance: U256::from(allowance),
            destination_shield_fee_bps: U256::from(shield_fee_bps),
        }),
    };
    let mut reviewed = bridge_review_for(delivery);
    reviewed.bridge = Some(quote(1_000, 100, 25));
    let approval = reviewed
        .approval(reviewed.suggested_private_minimum(), false)
        .unwrap();
    let bounds = &approval.bounds;
    assert_eq!(
        (
            bounds.destination_minimum,
            bounds.destination_shield_fee_bps,
            bounds.delivery_allowance
        ),
        (
            Some(U256::from(900)),
            Some(U256::from(25)),
            Some(U256::from(100))
        )
    );
    assert_eq!(reviewed.approval_change(&approval), None);

    let requoted = |quote| {
        let mut review = reviewed.clone();
        review.bridge = Some(quote);
        review
    };
    let redelivered = |delivery| {
        let mut review = reviewed.clone();
        review.plan.delivery = SwapDelivery::Bridge(delivery);
        review
    };
    for (fresh, change) in [
        (
            requoted(quote(1_000, 100, 30)),
            SwapReviewChange::DestinationShieldFee {
                approved: U256::from(25),
                current: U256::from(30),
            },
        ),
        // The same output with a higher allowance delivers less: the allowance is the cause.
        (
            requoted(quote(1_000, 150, 25)),
            SwapReviewChange::DeliveryAllowance {
                approved: U256::from(100),
                current: U256::from(150),
            },
        ),
        // A lower output at the approved allowance is the bridge quote's own shortfall.
        (
            requoted(quote(950, 100, 25)),
            SwapReviewChange::DestinationMinimum {
                approved: U256::from(900),
                current: U256::from(850),
            },
        ),
        (
            reviewed.with_receiver(Address::repeat_byte(0xec)),
            SwapReviewChange::Delivery,
        ),
        (
            redelivered(private_delivery(
                account,
                BridgeShieldFailure::KeepOnDestination,
            )),
            SwapReviewChange::Delivery,
        ),
    ] {
        assert_eq!(fresh.approval_change(&approval), Some(change), "{change:?}");
    }

    // An approval saved without a destination shield fee can't sign a private delivery.
    let mut unbound = approval.clone();
    unbound.bounds.destination_shield_fee_bps = None;
    assert_eq!(
        reviewed.approval_change(&unbound),
        Some(SwapReviewChange::DestinationShieldFee {
            approved: U256::ZERO,
            current: U256::from(25),
        })
    );
}

// A requote after setup that falls short of the approved minimums by at most a fifth of the
// approved allowed gas is signed at those minimums. A larger shortfall needs a new review. One
// wei is one buy unit here, so the rollup data cost sets the gas estimate directly.
#[test]
fn drift_within_a_fifth_of_the_approved_gas_keeps_the_approval() {
    let price_at = |delivery, buy_amount: U256, data_cost: U256, gas_price_wei: u128| {
        let quote: CowQuote = serde_json::from_value(serde_json::json!({
            "quote": {
                "sellToken": WETH, "buyToken": USDC,
                "sellAmount": "997500", "buyAmount": buy_amount.to_string(),
                "validTo": 1, "feeAmount": "2500", "gasAmount": "0", "gasPrice": "0",
                "sellTokenPrice": "1", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }))
        .unwrap();
        price_swap_review(
            plan_for(delivery),
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
            GAS_SHARE_BALANCED_BPS,
            Duration::from_mins(30),
            gas_price_wei,
            data_cost,
            OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
        )
        .unwrap()
    };
    let price = |delivery, buy_amount, data_cost| price_at(delivery, buy_amount, data_cost, 1);
    let (bought, data_cost) = (U256::from(10_000_000), U256::from(4_000_000));
    // Balanced deducts a quarter of the gas estimate, so four more wei allow one more unit.
    let gas_up = |allowance: U256| data_cost + U256::from(4) * allowance;

    for delivery in [
        SwapDelivery::Reshield,
        SwapDelivery::External {
            receiver: Address::repeat_byte(0x77),
        },
    ] {
        let reviewed = price(delivery, bought, data_cost);
        let approval = reviewed
            .approval(reviewed.suggested_private_minimum(), false)
            .unwrap();
        let (minimum, allowance) = (approval.bounds.private_minimum, reviewed.gas_allowance());
        let cushion = allowance / U256::from(5);
        // At its own minimum the order leaves room for exactly the allowed gas.
        assert_eq!(reviewed.gas_allowance_for(minimum).unwrap(), allowance);

        // More gas, one unit inside the cushion: the order keeps the approved minimum.
        let fresh = price(delivery, bought, gas_up(cushion - U256::ONE));
        assert_eq!(fresh.gas_allowance(), allowance + cushion - U256::ONE);
        assert!(fresh.suggested_private_minimum() < minimum);
        assert_eq!(fresh.approval_change(&approval), None, "{delivery:?}");
        assert_eq!(fresh.approved_order_minimum(&approval), Ok(minimum));
        // Two units beyond it, which the shield fee's rounding can't cover.
        let fresh = price(delivery, bought, gas_up(cushion + U256::from(2)));
        assert_eq!(
            fresh.approval_change(&approval),
            Some(SwapReviewChange::GasAllowance {
                approved: allowance,
                current: allowance + cushion + U256::from(2),
            }),
            "{delivery:?}"
        );

        // A lower quote at the same gas is taken from the allowed gas the same way.
        let fresh = price(delivery, bought - cushion / U256::from(2), data_cost);
        assert!(fresh.suggested_private_minimum() < minimum);
        assert_eq!(fresh.approved_order_minimum(&approval), Ok(minimum));
        // The order then leaves room for that much less gas.
        let shortfall = fresh.buy_amount_for(minimum).unwrap() - fresh.limit.buy_amount;
        assert_eq!(
            fresh.gas_allowance_for(minimum).unwrap(),
            fresh.gas_allowance() - shortfall,
            "{delivery:?}"
        );
        let fresh = price(delivery, bought - cushion * U256::from(2), data_cost);
        assert_eq!(
            fresh.approval_change(&approval),
            Some(SwapReviewChange::Minimum {
                approved: minimum,
                current: fresh.suggested_private_minimum(),
            }),
            "{delivery:?}"
        );

        // The shortfall must also fit the fresh allowed gas. With gas far lower, a quote lower
        // by the gas saved and half the cushion falls short by more than is allowed now.
        let low_gas = data_cost / U256::from(100);
        let saved = allowance - price_at(delivery, bought, low_gas, 0).gas_allowance();
        let fresh = price_at(
            delivery,
            bought - saved - cushion / U256::from(2),
            low_gas,
            0,
        );
        let shortfall = fresh.buy_amount_for(minimum).unwrap() - fresh.limit.buy_amount;
        assert!(
            fresh.gas_allowance() < shortfall && shortfall <= cushion,
            "{delivery:?}"
        );
        assert_eq!(
            fresh.approval_change(&approval),
            Some(SwapReviewChange::Minimum {
                approved: minimum,
                current: fresh.suggested_private_minimum(),
            }),
            "{delivery:?}"
        );

        // A better quote at lower gas keeps the approved minimum, and the order leaves room
        // for the allowed gas and the gain, here more than the whole gas estimate.
        let fresh = price_at(delivery, bought * U256::from(2), low_gas, 0);
        assert_eq!(fresh.approved_order_minimum(&approval), Ok(minimum));
        let gain = fresh.limit.buy_amount - fresh.buy_amount_for(minimum).unwrap();
        let room = fresh.gas_allowance_for(minimum).unwrap();
        assert_eq!(room, fresh.gas_allowance() + gain, "{delivery:?}");
        assert!(room > fresh.gas_estimate(), "{delivery:?}");
    }

    // A Bridge order buys its deposit. A bridge quote below the approved destination minimum
    // raises the deposit until the provider delivers that minimum again.
    let delivery = SwapDelivery::Bridge(near_delivery());
    let bridged = |review: &SwapReview, destination_minimum: u64| {
        let mut review = review.clone();
        review.bridge = Some(SwapBridgeQuote {
            provider: BridgeProvider::NearIntents,
            destination_minimum: U256::from(destination_minimum),
            expected_output: U256::from(destination_minimum),
            fee: Some(U256::ZERO),
            leg: BridgeLegPrice::SameAsset,
            fill_time_sec: None,
            private: None,
        });
        review
    };
    let reviewed = bridged(&price(delivery, bought, data_cost), 1_000_000);
    let deposit = reviewed.suggested_private_minimum();
    let approval = reviewed.approval(deposit, false).unwrap();
    let cushion = reviewed.gas_allowance() / U256::from(5);

    // 0.1% less on the destination: the deposit grows by that and the 5 bps margin.
    let scaled = (deposit * U256::from(1_000_000)).div_ceil(U256::from(999_000));
    let raised = scaled + (scaled * U256::from(5)).div_ceil(U256::from(10_000));
    assert!(raised > deposit && raised - deposit <= cushion);
    let fresh = bridged(&reviewed, 999_000);
    assert_eq!(fresh.approval_change(&approval), None);
    assert_eq!(fresh.approved_order_minimum(&approval), Ok(raised));
    // With more gas as well, the scaled deposit is below the approved one, which is kept.
    let fresh = bridged(
        &price(delivery, bought, gas_up(cushion / U256::from(2))),
        999_000,
    );
    assert!(fresh.suggested_private_minimum() < deposit);
    assert_eq!(fresh.approved_order_minimum(&approval), Ok(deposit));
    // Half the destination minimum would double the deposit.
    assert_eq!(
        bridged(&reviewed, 500_000).approval_change(&approval),
        Some(SwapReviewChange::DestinationMinimum {
            approved: U256::from(1_000_000),
            current: U256::from(500_000),
        })
    );
}
