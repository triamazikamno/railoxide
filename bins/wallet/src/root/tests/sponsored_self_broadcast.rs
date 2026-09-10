use super::*;
use crate::root::private_action::{
    BLOCK_BUILDER_SPONSORSHIP_LABEL, DeliveryMode, SelfBroadcastFundingMode, SponsoredAssetFee,
    SponsoredAuthorizationDisplay, SponsoredFundingEstimate, SponsoredFundingEstimateState,
    append_sponsorship_authorization_rows, apply_sponsored_authorization_review,
    effective_delivery_funding_mode, effective_self_broadcast_funding_mode,
    sponsored_authorization_display, sponsored_estimate_allows_submission,
    sponsored_estimate_failure_state, sponsored_funding_choice_visible,
    sponsored_incentive_from_text, sponsored_self_broadcast_availability_reason,
    sponsored_signer_balance_snapshot_changed,
};
use crate::root::spend_authorization::SpendAuthorizationSummaryRow;
use alloy::primitives::FixedBytes;
use wallet_ops::{
    SponsoredActionKind, SponsoredIncentive, SponsorshipError,
    settings::{build_effective_chain_configs, build_effective_token_registry},
    sponsored_authorization_limit, sponsorship_payment,
};

#[test]
fn sponsored_funding_choice_requires_a_configured_relay() {
    assert!(!sponsored_funding_choice_visible(None));
    assert_eq!(
        effective_self_broadcast_funding_mode(None, SelfBroadcastFundingMode::PrivateSponsorship),
        SelfBroadcastFundingMode::PublicBalance,
    );

    let settings = WalletSettings::default();
    let mut chain = build_effective_chain_configs(&settings)
        .expect("effective settings")
        .remove(&1)
        .expect("Ethereum config");
    assert!(sponsored_funding_choice_visible(Some(&chain)));
    assert_eq!(
        effective_self_broadcast_funding_mode(
            Some(&chain),
            SelfBroadcastFundingMode::PrivateSponsorship,
        ),
        SelfBroadcastFundingMode::PrivateSponsorship,
    );

    chain.sponsored_bundle_relays.clear();
    assert!(!sponsored_funding_choice_visible(Some(&chain)));
    assert_eq!(
        effective_self_broadcast_funding_mode(
            Some(&chain),
            SelfBroadcastFundingMode::PrivateSponsorship,
        ),
        SelfBroadcastFundingMode::PublicBalance,
    );
}

#[test]
fn delivery_mode_isolates_sponsorship_funding() {
    let settings = WalletSettings::default();
    let chain = build_effective_chain_configs(&settings)
        .expect("effective settings")
        .remove(&1)
        .expect("Ethereum config");

    assert_eq!(
        effective_delivery_funding_mode(
            DeliveryMode::SelfBroadcast,
            Some(&chain),
            SelfBroadcastFundingMode::PrivateSponsorship,
        ),
        SelfBroadcastFundingMode::PrivateSponsorship,
    );
    for delivery_mode in [
        DeliveryMode::PublicBroadcaster,
        DeliveryMode::ManualCalldata,
    ] {
        assert_eq!(
            effective_delivery_funding_mode(
                delivery_mode,
                Some(&chain),
                SelfBroadcastFundingMode::PrivateSponsorship,
            ),
            SelfBroadcastFundingMode::PublicBalance,
        );
    }
}

#[test]
fn sponsored_funding_reports_each_static_prerequisite() {
    assert!(sponsored_self_broadcast_availability_reason(None).is_some());

    let settings = WalletSettings::default();
    let mut chain = build_effective_chain_configs(&settings)
        .expect("effective settings")
        .remove(&1)
        .expect("Ethereum config");
    assert_eq!(
        sponsored_self_broadcast_availability_reason(Some(&chain)),
        None
    );

    chain.sponsored_bundle_relays.clear();
    assert!(sponsored_self_broadcast_availability_reason(Some(&chain)).is_some());
    chain = build_effective_chain_configs(&settings)
        .expect("effective settings")
        .remove(&1)
        .expect("Ethereum config");
    chain.wrapped_native_token = None;
    assert!(sponsored_self_broadcast_availability_reason(Some(&chain)).is_some());
    chain.wrapped_native_token = Some("0x0000000000000000000000000000000000000001".into());
    chain.coinbase_payer = None;
    assert!(sponsored_self_broadcast_availability_reason(Some(&chain)).is_some());
}

#[test]
fn custom_incentive_control_accepts_only_integer_percent_bounds() {
    assert_eq!(
        sponsored_incentive_from_text(SponsoredIncentive::Standard, "invalid"),
        Ok(SponsoredIncentive::Standard)
    );
    assert_eq!(
        sponsored_incentive_from_text(SponsoredIncentive::Custom(25), "1"),
        Ok(SponsoredIncentive::Custom(1))
    );
    assert_eq!(
        sponsored_incentive_from_text(SponsoredIncentive::Custom(25), "100"),
        Ok(SponsoredIncentive::Custom(100))
    );
    assert!(sponsored_incentive_from_text(SponsoredIncentive::Custom(25), "0").is_err());
    assert!(sponsored_incentive_from_text(SponsoredIncentive::Custom(25), "101").is_err());
    assert!(sponsored_incentive_from_text(SponsoredIncentive::Custom(25), "1.5").is_err());
}

#[test]
fn sponsorship_summary_requires_fresh_explicit_review_without_warning_notes() {
    let summary = apply_sponsored_authorization_review(
        SpendAuthorizationSummary::new("Provisional", "Review sponsorship", Vec::new()),
        SelfBroadcastFundingMode::PrivateSponsorship,
    );

    assert!(summary.warnings_for_test().is_empty());
    assert!(!spend_authorization_can_use_cached_password(&summary));
}

fn sponsored_balance_snapshot(amount: PublicBalanceAmount) -> PublicBalanceSnapshot {
    PublicBalanceSnapshot {
        chain_id: 1,
        refreshed_at: SystemTime::UNIX_EPOCH,
        accounts: vec![PublicAccountBalance {
            observed_at: None,
            observed_block: None,
            account: PublicAccountMetadata {
                public_account_uuid: "sponsored-signer".to_string(),
                address: Address::from([0x44; 20]),
                label: None,
                source: PublicAccountSource::Imported,
                scope: PublicAccountScope::Global,
                derivation_index: None,
                hardware_descriptor: None,
                status: PublicAccountStatus::Active,
                display_order: 0,
            },
            balances: vec![PublicBalanceEntry {
                asset: PublicBalanceAsset {
                    id: PublicAssetId::Native,
                    symbol: "ETH".to_string(),
                    decimals: 18,
                },
                amount,
            }],
        }],
    }
}

#[test]
fn sponsored_balance_dependency_changes_only_when_snapshot_value_changes() {
    let five = sponsored_balance_snapshot(PublicBalanceAmount::Available(U256::from(5_u8)));
    let same = sponsored_balance_snapshot(PublicBalanceAmount::Available(U256::from(5_u8)));
    let six = sponsored_balance_snapshot(PublicBalanceAmount::Available(U256::from(6_u8)));
    let unavailable = sponsored_balance_snapshot(PublicBalanceAmount::Unavailable);

    assert!(!sponsored_signer_balance_snapshot_changed(
        Some(&five),
        &same,
        1,
        "sponsored-signer",
    ));
    assert!(sponsored_signer_balance_snapshot_changed(
        Some(&five),
        &six,
        1,
        "sponsored-signer",
    ));
    assert!(sponsored_signer_balance_snapshot_changed(
        Some(&five),
        &unavailable,
        1,
        "sponsored-signer",
    ));
}

#[test]
fn sponsored_estimate_redacts_unclassified_quote_failures() {
    let error = eyre::eyre!("planner internals");

    assert_eq!(
        sponsored_estimate_failure_state(1, None, &error),
        SponsoredFundingEstimateState::Unavailable
    );
}

#[test]
fn sponsored_submission_accepts_a_ready_estimate_during_background_refresh() {
    let payment =
        sponsorship_payment(100, 2, U256::ZERO, SponsoredIncentive::Standard).expect("payment");
    let ready = SponsoredFundingEstimateState::Ready(Box::new(SponsoredFundingEstimate {
        chain_id: 1,
        wrapped_native_token: Address::ZERO,
        expected_payment: payment,
        maximum_payment: payment,
        primary_unshield_protocol_fee: None,
    }));

    assert!(sponsored_estimate_allows_submission(
        SelfBroadcastFundingMode::PublicBalance,
        None,
        false,
    ));
    assert!(!sponsored_estimate_allows_submission(
        SelfBroadcastFundingMode::PrivateSponsorship,
        None,
        false,
    ));
    assert!(sponsored_estimate_allows_submission(
        SelfBroadcastFundingMode::PrivateSponsorship,
        Some(&ready),
        false,
    ));
    assert!(sponsored_estimate_allows_submission(
        SelfBroadcastFundingMode::PrivateSponsorship,
        Some(&ready),
        true,
    ));
    assert!(!sponsored_estimate_allows_submission(
        SelfBroadcastFundingMode::PrivateSponsorship,
        Some(&SponsoredFundingEstimateState::Unavailable),
        false,
    ));
}

#[test]
fn sponsored_estimate_preserves_insufficient_wrapped_native_amounts() {
    let token = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let error: eyre::Report = SponsorshipError::InsufficientWrappedNative {
        available: U256::from(5_u64),
        required: U256::from(8_u64),
    }
    .into();

    assert_eq!(
        sponsored_estimate_failure_state(1, Some(token), &error),
        SponsoredFundingEstimateState::InsufficientWrappedNative {
            chain_id: 1,
            token: Some(token),
            available: U256::from(5_u64),
            required: U256::from(8_u64),
        }
    );
}

#[test]
fn sponsored_estimate_does_not_present_an_intermediate_required_amount() {
    let token = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let error: eyre::Report = SponsorshipError::InsufficientWrappedNativeForQuote {
        available: U256::from(5_u64),
    }
    .into();

    assert_eq!(
        sponsored_estimate_failure_state(1, Some(token), &error),
        SponsoredFundingEstimateState::InsufficientWrappedNativeForQuote {
            chain_id: 1,
            token: Some(token),
            available: U256::from(5_u64),
        }
    );
}

#[test]
fn sponsored_display_accounting_reconciles_expected_cost() {
    let expected_payment = sponsorship_payment(100, 1, U256::ZERO, SponsoredIncentive::Standard)
        .expect("expected payment");
    let maximum_payment = sponsorship_payment(100, 2, U256::ZERO, SponsoredIncentive::Standard)
        .expect("maximum payment");
    let estimate = SponsoredFundingEstimate {
        chain_id: 1,
        wrapped_native_token: Address::ZERO,
        expected_payment,
        maximum_payment,
        primary_unshield_protocol_fee: Some(SponsoredAssetFee {
            token: Address::from([0x11; 20]),
            amount: U256::from(7_500_u64),
        }),
    };

    assert_eq!(
        maximum_payment.reimbursement_base,
        maximum_payment.outer_gas_cap + maximum_payment.funding_gas_cap
    );
    assert_eq!(
        estimate.builder_premium(),
        maximum_payment.gross_wrapped_native_spend - maximum_payment.reimbursement_base
    );
    assert_eq!(
        estimate.expected_excess_deposit(),
        maximum_payment.funding_principal - expected_payment.outer_gas_cap
    );
    assert_eq!(
        estimate.expected_network_gas_cost(),
        expected_payment.outer_gas_cap + maximum_payment.funding_gas_cap
    );
    assert_eq!(
        estimate.expected_sponsorship_cost(),
        maximum_payment.gross_wrapped_native_spend - estimate.expected_excess_deposit()
    );
    assert_eq!(
        estimate.expected_sponsorship_cost(),
        estimate.expected_network_gas_cost() + estimate.builder_premium()
    );
}

#[test]
fn sponsorship_summary_contains_only_user_relevant_formatted_maxima() {
    let display = SponsoredAuthorizationDisplay {
        gross_wrapped_native_spend: "Up to 0.0386 WETH · $100.00".to_owned(),
        max_fee_per_gas: "1.02677161 gwei".to_owned(),
        max_priority_fee_per_gas: "0.831059755 gwei".to_owned(),
    };
    let mut rows = Vec::new();

    append_sponsorship_authorization_rows(
        &mut rows,
        SelfBroadcastFundingMode::PrivateSponsorship,
        SponsoredIncentive::Standard,
        Some(&display),
        Some("Account #6"),
    );
    let rows = rows
        .iter()
        .map(SpendAuthorizationSummaryRow::values_for_test)
        .collect::<Vec<_>>();

    assert_eq!(
        rows,
        vec![
            (
                "Gas funding".to_owned(),
                BLOCK_BUILDER_SPONSORSHIP_LABEL.to_owned()
            ),
            ("Builder incentive".to_owned(), "5%".to_owned()),
            ("Transaction signer".to_owned(), "Account #6".to_owned()),
            (
                "Gross sponsorship spend".to_owned(),
                display.gross_wrapped_native_spend,
            ),
            ("Maximum fee per gas".to_owned(), display.max_fee_per_gas),
            (
                "Maximum priority fee per gas".to_owned(),
                display.max_priority_fee_per_gas,
            ),
        ]
    );
}

#[test]
fn sponsorship_display_scales_token_usd_and_gwei_values() {
    let settings = WalletSettings::default();
    let registry = build_effective_token_registry(&settings).expect("effective token registry");
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, U256::from(3_000_000_000_u64));
    let weth: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
        .parse()
        .expect("WETH address");
    let limit = sponsored_authorization_limit(
        FixedBytes::from([0x30; 32]),
        1_000_000,
        SponsoredActionKind::Send,
        weth,
        Address::from([0x32; 20]),
        Address::from([0x34; 20]),
        1_250_000_000,
        100_000_000,
        U256::ZERO,
        SponsoredIncentive::Standard,
        Address::from([0x33; 20]),
        U256::ZERO,
    )
    .expect("authorization limit");

    let display = sponsored_authorization_display(1, limit, &registry, &cache);

    assert_eq!(
        display.gross_wrapped_native_spend,
        "Up to 0.001344 WETH · $4.03"
    );
    assert_eq!(display.max_fee_per_gas, "1.25 gwei");
    assert_eq!(display.max_priority_fee_per_gas, "0.1 gwei");
}
