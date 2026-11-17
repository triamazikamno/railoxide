use super::*;
use alloy::primitives::U256;

#[test]
fn picker_estimates_allow_blank_amount_without_relaxing_transaction_validation() {
    for output in [
        PrivateEstimateOutput::Send,
        PrivateEstimateOutput::Unshield {
            unwrap: false,
            native_top_up: None,
        },
    ] {
        let validate = match output {
            PrivateEstimateOutput::Send => send_public_broadcaster_estimate_input_error,
            PrivateEstimateOutput::Unshield { .. } => {
                unshield_public_broadcaster_estimate_input_error
            }
        };
        let mut input = PrivateEstimateInput {
            custom_fee_amount: None,
            asset: UnshieldAsset {
                chain_id: 1,
                token: Address::ZERO,
                label: "TEST".into(),
                decimals: Some(18),
                total: U256::from(9_000_000_000_000_000_000_u64),
                poi_verified_total: U256::from(7_000_000_000_000_000_000_u64),
                max_batched: U256::from(5_446_683_954_612_232_014_u64),
                icon_path: None,
            },
            recipient: String::new(),
            amount: " ".into(),
            broadcaster: BroadcasterChoice::Specific {
                railgun_address: "unavailable broadcaster".into(),
            },
            shown_broadcaster: None,
            refused_broadcasters: Vec::new(),
            input_pin: None,
            executor_operation: None,
            fee_token: Address::ZERO,
            fee_mode: FeeHandlingMode::DeductFromAmount,
            allow_out_of_range: false,
            favorites_only: false,
            output,
        };
        assert!(validate("", &input.amount, &input.asset).is_some());
        let picker = input.clone().for_picker(String::new());
        assert!(validate("", &picker.amount, &picker.asset).is_none());
        assert_eq!(
            parse_send_amount(&picker.amount, picker.asset.decimals).unwrap(),
            input.asset.max_batched
        );
        let policy = wallet_ops::BroadcasterFeePolicy::default();
        let candidates = wallet_ops::public_broadcaster_candidates_for_asset(
            &[crate::root::tests::fee_row(1, input.fee_token, "available")],
            1,
            input.fee_token,
            policy,
            None,
        )
        .unwrap();
        let trust = wallet_ops::PublicBroadcasterTrustFilter::default();
        assert!(
            select_public_broadcaster_with_policy_and_trust(
                &candidates,
                &WalletRoot::public_broadcaster_selection(&input.broadcaster),
                policy,
                &trust,
            )
            .is_err()
        );
        assert!(
            select_public_broadcaster_with_policy_and_trust(
                &candidates,
                &WalletRoot::public_broadcaster_selection(&picker.broadcaster),
                policy,
                &trust,
            )
            .is_ok()
        );
        input.amount = "1.123456789012345678".into();
        let picker = input.clone().for_picker(String::new());
        assert_eq!(picker.amount, input.amount);
        input.amount = "invalid".into();
        let picker = input.for_picker(String::new());
        assert!(validate("", &picker.amount, &picker.asset).is_some());
    }
}

#[test]
fn random_estimates_keep_the_shown_broadcaster_while_it_is_eligible() {
    let policy = wallet_ops::BroadcasterFeePolicy::default();
    let candidates = wallet_ops::public_broadcaster_candidates_for_asset(
        &[crate::root::tests::fee_row(1, Address::ZERO, "shown")],
        1,
        Address::ZERO,
        policy,
        None,
    )
    .unwrap();
    let shown = candidates[0].railgun_address.as_str();
    let trust = wallet_ops::PublicBroadcasterTrustFilter::default();
    let select = |choice: &BroadcasterChoice, shown: Option<&str>| {
        estimate_broadcaster_selection(choice, shown, &candidates, policy, &trust)
    };
    let specific = |railgun_address: &str| PublicBroadcasterSelection::Specific {
        railgun_address: railgun_address.into(),
    };
    assert_eq!(
        select(&BroadcasterChoice::Random, Some(shown)),
        specific(shown)
    );
    // An ineligible shown broadcaster falls back to a fresh random pick.
    assert_eq!(
        select(&BroadcasterChoice::Random, Some("gone")),
        PublicBroadcasterSelection::Random
    );
    let chosen = BroadcasterChoice::Specific {
        railgun_address: "chosen".into(),
    };
    assert_eq!(select(&chosen, Some(shown)), specific("chosen"));
}

#[test]
fn random_estimates_skip_refused_broadcasters_unless_none_remain() {
    const OTHER: &str = "0zk1qykzjxctynyz4z43pukckpv43jyzhyvy0ehrd5wuc54l5enqf9qfrrv7j6fe3z53la7enqphqvxys9aqyp9xx0km95ehqslx8apmu8l7anc7emau4tvsultrkvd";
    let policy = wallet_ops::BroadcasterFeePolicy::default();
    let refused_row = crate::root::tests::fee_row(1, Address::ZERO, "refused");
    let refused = refused_row.railgun_address.to_string();
    let mut other_row = crate::root::tests::fee_row(1, Address::ZERO, "other");
    other_row.railgun_address = Arc::from(OTHER);
    let candidates = wallet_ops::public_broadcaster_candidates_for_asset(
        &[refused_row, other_row],
        1,
        Address::ZERO,
        policy,
        None,
    )
    .unwrap();
    assert_eq!(candidates.len(), 2);
    let trust = PublicBroadcasterTrustFilter::default();
    let filter = exclude_refused_broadcasters(
        trust.clone(),
        std::slice::from_ref(&refused),
        &candidates,
        policy,
    );
    let picked = select_public_broadcaster_with_policy_and_trust(
        &candidates,
        &PublicBroadcasterSelection::Random,
        policy,
        &filter,
    )
    .unwrap();
    assert_eq!(picked.railgun_address, OTHER);
    // Once every candidate has refused, the plain filter keeps an estimate possible.
    let filter = exclude_refused_broadcasters(trust, &[refused, OTHER.into()], &candidates, policy);
    assert_eq!(
        filter.preferences,
        wallet_ops::vault::BroadcasterPreferences::default()
    );
}
