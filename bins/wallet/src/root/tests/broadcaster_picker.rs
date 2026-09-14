use super::*;
use crate::root::broadcaster_picker::{
    BroadcasterPickerContent, broadcaster_picker_dialog_vertical_geometry,
    clear_pending_content_if_current, invalidate_broadcaster_picker_live_update,
    take_pending_broadcaster_picker_live_update,
};

fn candidate_with_fee(fee: U256, anchor: Option<U256>) -> PublicBroadcasterCandidate {
    candidate_with_fee_policy(fee, anchor, BroadcasterFeePolicy::default())
}

fn candidate_with_fee_policy(
    fee: U256,
    anchor: Option<U256>,
    policy: BroadcasterFeePolicy,
) -> PublicBroadcasterCandidate {
    let token = Address::from([0x91; 20]);
    let mut row = fee_row(1, token, "picker-status");
    row.fee = fee;
    public_broadcaster_candidates_for_asset(&[row], 1, token, None, policy, anchor)
        .expect("picker candidate")
        .remove(0)
}

fn picker_content(query: &str) -> BroadcasterPickerContent {
    BroadcasterPickerContent {
        entries: Vec::new(),
        empty_message: SharedString::from("No broadcasters"),
        generating: false,
        show_all_broadcasters: false,
        query: query.to_string(),
        selected_address: None,
        view_mode: BroadcasterPickerViewMode::Grouped,
        expanded_groups: BTreeSet::new(),
        collapsed_selected_children: BTreeMap::new(),
    }
}

#[test]
fn picker_fee_status_maps_policy_without_changing_eligibility() {
    let policy = BroadcasterFeePolicy::default();
    let in_range = candidate_with_fee(U256::from(105), Some(U256::from(100)));
    let no_premium = candidate_with_fee(U256::from(100), Some(U256::from(100)));
    let low = candidate_with_fee(U256::from(95), Some(U256::from(100)));
    let very_low = candidate_with_fee(U256::from(80), Some(U256::from(100)));
    let high = candidate_with_fee(U256::from(160), Some(U256::from(100)));
    let not_assessed = candidate_with_fee(U256::from(100), None);

    assert_eq!(
        broadcaster_picker_fee_status(&in_range, policy),
        BroadcasterPickerFeeStatus::InRange
    );
    assert_eq!(
        broadcaster_picker_fee_status(&no_premium, policy),
        BroadcasterPickerFeeStatus::NoPremium
    );
    assert_eq!(
        broadcaster_picker_fee_status(&low, policy),
        BroadcasterPickerFeeStatus::LowIncentive
    );
    assert_eq!(
        broadcaster_picker_fee_status(&very_low, policy),
        BroadcasterPickerFeeStatus::VeryLowIncentive
    );
    assert_eq!(
        broadcaster_picker_fee_status(&high, policy),
        BroadcasterPickerFeeStatus::HighFee
    );
    assert_eq!(
        broadcaster_picker_fee_status(&not_assessed, policy),
        BroadcasterPickerFeeStatus::NotAssessed
    );
    assert_eq!(
        BroadcasterPickerFeeStatus::InRange.tier(),
        BroadcasterPickerTier::Incentivised
    );
    assert_eq!(
        BroadcasterPickerFeeStatus::NoPremium.tier(),
        BroadcasterPickerTier::Uncompensated
    );
    assert_eq!(
        BroadcasterPickerFeeStatus::LowIncentive.tier(),
        BroadcasterPickerTier::Uncompensated
    );
    assert_eq!(
        BroadcasterPickerFeeStatus::VeryLowIncentive.tier(),
        BroadcasterPickerTier::OutsideRange
    );
    assert_eq!(
        BroadcasterPickerFeeStatus::HighFee.tier(),
        BroadcasterPickerTier::OutsideRange
    );
    assert_eq!(
        BroadcasterPickerFeeStatus::NotAssessed.tier(),
        BroadcasterPickerTier::NotAssessed
    );
    assert_eq!(
        BroadcasterPickerTier::Incentivised.badge_label(false),
        Some("Incentivised")
    );
    assert_eq!(
        BroadcasterPickerTier::Uncompensated.badge_label(false),
        None
    );
    assert_eq!(
        BroadcasterPickerTier::Uncompensated.badge_label(true),
        Some("No fee")
    );
    assert!(BroadcasterPickerTier::Uncompensated.is_muted());
    assert!(BroadcasterPickerTier::OutsideRange.is_muted());
    assert!(BroadcasterPickerTier::NotAssessed.is_muted());
    assert!(!BroadcasterPickerTier::Incentivised.is_muted());

    assert!(no_premium.is_allowed_by_fee_policy(policy));
    assert!(low.is_allowed_by_fee_policy(policy));
    assert!(!very_low.is_allowed_by_fee_policy(policy));
    assert!(!high.is_allowed_by_fee_policy(policy));
    assert!(not_assessed.is_allowed_by_fee_policy(policy));
    assert!(very_low.is_allowed_by_fee_policy(policy.with_allow_suspicious_broadcasters(true)));
}

#[test]
fn picker_selection_admission_respects_override_and_rejects_stale_candidate() {
    let policy = BroadcasterFeePolicy::default();
    let candidate = candidate_with_fee(U256::from(160), Some(U256::from(100)));
    let choice = BroadcasterChoice::Specific {
        railgun_address: candidate.railgun_address.clone(),
    };

    assert!(!broadcaster_choice_supported_by_candidates(
        &choice,
        std::slice::from_ref(&candidate),
        policy,
    ));
    let override_policy = policy.with_allow_suspicious_broadcasters(true);
    assert!(broadcaster_choice_supported_by_candidates(
        &choice,
        std::slice::from_ref(&candidate),
        override_policy,
    ));

    let stale_choice = BroadcasterChoice::Specific {
        railgun_address: "missing-broadcaster".to_string(),
    };
    assert!(!broadcaster_choice_supported_by_candidates(
        &stale_choice,
        &[candidate],
        override_policy,
    ));
}

#[test]
fn picker_fee_status_details_use_plain_gas_cost_language() {
    let policy = BroadcasterFeePolicy::default();
    let in_range = candidate_with_fee(U256::from(105), Some(U256::from(100)));
    let no_premium = candidate_with_fee(U256::from(100), Some(U256::from(100)));
    let low = candidate_with_fee(U256::from(95), Some(U256::from(100)));
    let very_low = candidate_with_fee(U256::from(80), Some(U256::from(100)));
    let high = candidate_with_fee(U256::from(160), Some(U256::from(100)));
    let unknown = candidate_with_fee(U256::from(123), None);
    let unrepresentably_high = candidate_with_fee(U256::MAX, Some(U256::ONE));

    assert_eq!(
        broadcaster_picker_fee_status_detail(&in_range, policy),
        "Charges more than the gas it spends, so submitting your transaction earns them something."
    );
    let no_premium_detail = broadcaster_picker_fee_status_detail(&no_premium, policy);
    assert!(no_premium_detail.contains("gas cost or less"));
    assert!(no_premium_detail.contains("earns nothing"));
    assert!(!no_premium_detail.contains("fee anchor"));
    let tiny_positive = candidate_with_fee(U256::from(1_000_001), Some(U256::from(1_000_000)));
    assert_eq!(
        broadcaster_picker_fee_status(&tiny_positive, policy),
        BroadcasterPickerFeeStatus::InRange
    );
    let tiny_positive_detail = broadcaster_picker_fee_status_detail(&tiny_positive, policy);
    assert!(tiny_positive_detail.contains("earns them something"));
    let tiny_negative = candidate_with_fee(U256::from(999_999), Some(U256::from(1_000_000)));
    assert_eq!(
        broadcaster_picker_fee_status(&tiny_negative, policy),
        BroadcasterPickerFeeStatus::LowIncentive
    );
    assert_eq!(
        BroadcasterPickerFeeStatus::LowIncentive.tier(),
        BroadcasterPickerTier::Uncompensated
    );
    let low_detail = broadcaster_picker_fee_status_detail(&low, policy);
    assert!(low_detail.contains("gas cost or less"));
    assert!(!low_detail.contains('%'));
    assert_eq!(
        broadcaster_picker_fee_status_detail(&very_low, policy),
        "This fee is below the allowed range."
    );
    assert_eq!(
        broadcaster_picker_fee_status_detail(&high, policy),
        "This fee is above the allowed range."
    );
    assert_eq!(
        broadcaster_picker_fee_status(&unrepresentably_high, policy),
        BroadcasterPickerFeeStatus::HighFee
    );
    let unrepresentable_detail =
        broadcaster_picker_fee_status_detail(&unrepresentably_high, policy);
    assert!(unrepresentable_detail.contains("outside the allowed range"));
    assert!(unrepresentable_detail.contains("gas-cost comparison is unavailable"));
    assert!(!unrepresentable_detail.contains("above"));
    assert!(!unrepresentable_detail.contains("raw token units"));
    assert!(broadcaster_picker_fee_status_detail(&unknown, policy).contains("123 raw token units"));
    assert!(
        broadcaster_picker_fee_status_detail(&unknown, policy)
            .contains("gas-cost comparison is unavailable")
    );
}

#[test]
fn suspicious_picker_status_uses_policy_boundaries_instead_of_premium_sign() {
    let above_anchor_window = BroadcasterFeePolicy {
        min_anchor_bps: 12_000,
        max_anchor_bps: 15_000,
        allow_suspicious_broadcasters: true,
    };
    let positive_but_below =
        candidate_with_fee_policy(U256::from(110), Some(U256::from(100)), above_anchor_window);
    assert_eq!(
        broadcaster_picker_fee_status(&positive_but_below, above_anchor_window),
        BroadcasterPickerFeeStatus::VeryLowIncentive
    );
    let below_detail =
        broadcaster_picker_fee_status_detail(&positive_but_below, above_anchor_window);
    assert_eq!(below_detail, "This fee is below the allowed range.");

    let below_anchor_window = BroadcasterFeePolicy {
        min_anchor_bps: 5_000,
        max_anchor_bps: 8_000,
        allow_suspicious_broadcasters: true,
    };
    let negative_but_above =
        candidate_with_fee_policy(U256::from(90), Some(U256::from(100)), below_anchor_window);
    assert_eq!(
        broadcaster_picker_fee_status(&negative_but_above, below_anchor_window),
        BroadcasterPickerFeeStatus::HighFee
    );
    let above_detail =
        broadcaster_picker_fee_status_detail(&negative_but_above, below_anchor_window);
    assert_eq!(above_detail, "This fee is above the allowed range.");
}

#[test]
fn matching_live_content_clears_an_older_pending_update() {
    let current = picker_content("a");
    let queued = picker_content("b");
    let incoming = current.clone();
    let mut pending = Some(queued.clone());

    assert!(clear_pending_content_if_current(
        current == incoming,
        &mut pending
    ));
    assert!(pending.is_none());

    pending = Some(queued.clone());
    assert!(!clear_pending_content_if_current(false, &mut pending));
    assert!(pending.as_ref() == Some(&queued));
}

#[test]
fn picker_fee_estimate_retry_state_deduplicates_and_rejects_stale_timers() {
    let mut retry = BroadcasterPickerFeeEstimateRetryState::default();
    assert!(retry.should_schedule(false, false, false));
    assert_eq!(retry.mark_scheduled(7), Duration::from_secs(1));
    assert!(retry.is_scheduled());
    assert!(!retry.should_schedule(false, false, false));
    assert!(!retry.clear_if_current(6));
    assert!(retry.is_scheduled());
    assert!(retry.clear_if_current(7));
    assert!(!retry.is_scheduled());

    assert_eq!(retry.mark_scheduled(8), Duration::from_secs(2));
    retry.finish_attempt(false);
    assert_eq!(retry.mark_scheduled(9), Duration::from_secs(4));
    retry.finish_attempt(true);
    assert!(!retry.clear_if_current(9));
    assert!(retry.should_schedule(false, false, false));
    assert!(!retry.should_schedule(true, false, false));
    assert!(!retry.should_schedule(false, true, false));
    assert!(retry.should_schedule(false, true, true));
    assert_eq!(retry.mark_scheduled(10), Duration::from_secs(1));
}

#[test]
fn scroll_for_more_visibility_tracks_remaining_list_offset() {
    assert!(!broadcaster_picker_scroll_hint_visible(px(0.0), px(0.0)));
    assert!(broadcaster_picker_scroll_hint_visible(px(0.0), px(100.0)));
    assert!(broadcaster_picker_scroll_hint_visible(px(-50.0), px(100.0)));
    assert!(!broadcaster_picker_scroll_hint_visible(
        px(-99.5),
        px(100.0)
    ));
    assert!(!broadcaster_picker_scroll_hint_visible(
        px(-100.0),
        px(100.0)
    ));
}

#[test]
fn picker_dialog_height_preserves_symmetric_available_margins_without_a_cap() {
    for (available_height, expected_margin, expected_height) in
        [(800.0, 80.0, 640.0), (1_600.0, 160.0, 1_280.0)]
    {
        let available_height = px(available_height);
        let (margin, dialog_height) = broadcaster_picker_dialog_vertical_geometry(available_height);

        assert_eq!(margin, px(expected_margin));
        assert_eq!(dialog_height, px(expected_height));
        assert_eq!(margin * 2.0 + dialog_height, available_height);
    }
}

#[test]
fn synchronous_live_update_invalidation_advances_epoch_and_clears_schedule() {
    let mut epoch = 4;
    let mut scheduled = true;

    invalidate_broadcaster_picker_live_update(&mut epoch, &mut scheduled);

    assert_eq!(epoch, 5);
    assert!(!scheduled);
}

#[test]
fn stale_live_update_callback_preserves_newer_pending_cycle() {
    let mut scheduled = true;
    let mut pending = Some("newer content");

    assert_eq!(
        take_pending_broadcaster_picker_live_update(4, 5, &mut scheduled, &mut pending),
        None
    );
    assert!(scheduled);
    assert_eq!(pending, Some("newer content"));
}

#[test]
fn current_live_update_callback_consumes_pending_content() {
    let mut scheduled = true;
    let mut pending = Some("current content");

    assert_eq!(
        take_pending_broadcaster_picker_live_update(5, 5, &mut scheduled, &mut pending),
        Some("current content")
    );
    assert!(!scheduled);
    assert!(pending.is_none());
}
