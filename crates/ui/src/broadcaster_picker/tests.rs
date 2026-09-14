use super::*;

fn picker_row(
    id: &str,
    fee: u64,
    status: BroadcasterPickerFeeStatus,
    sort_order: usize,
) -> BroadcasterPickerRow {
    let amount = U256::from(fee * 10);
    let premium_bps = match status {
        BroadcasterPickerFeeStatus::InRange => Some(500),
        BroadcasterPickerFeeStatus::NoPremium => Some(0),
        BroadcasterPickerFeeStatus::LowIncentive => Some(-500),
        BroadcasterPickerFeeStatus::VeryLowIncentive => Some(-2_000),
        BroadcasterPickerFeeStatus::HighFee => Some(6_000),
        BroadcasterPickerFeeStatus::NotAssessed => None,
    };
    BroadcasterPickerRow {
        railgun_address: id.to_string(),
        label: id.to_string(),
        advertised_fee: U256::from(fee),
        premium_bps,
        sort_order,
        estimated_fee_amount: Some(amount),
        estimated_fee_label: format!("{amount} TKN"),
        estimated_fee_usd_micro: Some(amount),
        estimated_fee_usd_label: Some(format!("${amount}")),
        fee_status: status,
        fee_tier: status.tier(),
        show_uncompensated_badge: false,
        fee_status_detail: format!("{} detail", status.tier().label()),
        fee_warning: matches!(
            status,
            BroadcasterPickerFeeStatus::VeryLowIncentive | BroadcasterPickerFeeStatus::HighFee
        )
        .then(|| "Fee outside allowed range".to_string()),
        favorite: false,
        selected: false,
        child_of: None,
    }
}

fn grouped(rows: &[BroadcasterPickerRow]) -> Vec<BroadcasterPickerEntry> {
    project_broadcaster_picker_entries(
        rows,
        BroadcasterPickerViewMode::Grouped,
        false,
        &BTreeSet::new(),
        &BTreeMap::new(),
    )
}

fn picker_group(
    entries: &[BroadcasterPickerEntry],
    key: BroadcasterPickerGroupKey,
) -> BroadcasterPickerGroup {
    entries
        .iter()
        .find_map(|entry| match entry {
            BroadcasterPickerEntry::Group(group) if group.key == key => Some(group.clone()),
            _ => None,
        })
        .expect("picker group")
}

#[test]
fn picker_fee_text_colors_preserve_each_cell_hierarchy() {
    assert_eq!(
        broadcaster_picker_fee_text_colors(false),
        (crate::theme::TEXT, crate::theme::TEXT_MUTED)
    );
    assert_eq!(
        broadcaster_picker_fee_text_colors(true),
        (crate::theme::TEXT_MUTED, crate::theme::TEXT_SUBTLE)
    );
}

#[test]
fn grouped_projection_orders_tiers_and_groups_the_entire_uncompensated_population() {
    let rows = vec![
        picker_row("low", 1, BroadcasterPickerFeeStatus::LowIncentive, 0),
        picker_row("positive-b", 10, BroadcasterPickerFeeStatus::InRange, 1),
        picker_row("zero", 8, BroadcasterPickerFeeStatus::NoPremium, 2),
        picker_row("positive-a", 5, BroadcasterPickerFeeStatus::InRange, 3),
        picker_row(
            "below-a",
            2,
            BroadcasterPickerFeeStatus::VeryLowIncentive,
            4,
        ),
        picker_row(
            "below-b",
            3,
            BroadcasterPickerFeeStatus::VeryLowIncentive,
            5,
        ),
        picker_row("unknown", 4, BroadcasterPickerFeeStatus::NotAssessed, 6),
    ];
    let entries = grouped(&rows);
    let uncompensated_key = BroadcasterPickerGroupKey::Tier(BroadcasterPickerTier::Uncompensated);

    assert!(matches!(
        entries.first(),
        Some(BroadcasterPickerEntry::Broadcaster(row))
            if row.railgun_address == "positive-a"
    ));
    assert!(matches!(
        entries.get(1),
        Some(BroadcasterPickerEntry::Broadcaster(row))
            if row.railgun_address == "positive-b"
    ));
    let uncompensated = picker_group(&entries, uncompensated_key);
    assert_eq!(uncompensated.count, 2);
    assert_eq!(uncompensated.fee_tier, BroadcasterPickerTier::Uncompensated);
    assert_eq!(uncompensated.label, "2 broadcasters earning no fee");
    assert_eq!(uncompensated.estimated_fee_label, "from 10 TKN");
    assert_eq!(
        uncompensated.estimated_fee_usd_label.as_deref(),
        Some("from $10")
    );
    assert!(uncompensated.detail.contains("gas cost or less"));
    assert!(!uncompensated.detail.contains("fee anchor"));
    assert_eq!(
        entries.iter().position(|entry| matches!(
            entry,
            BroadcasterPickerEntry::Group(group) if group.key == uncompensated_key
        )),
        Some(2)
    );
    let expanded = project_broadcaster_picker_entries(
        &rows,
        BroadcasterPickerViewMode::Grouped,
        false,
        &BTreeSet::from([uncompensated_key]),
        &BTreeMap::new(),
    );
    assert_eq!(
        expanded
            .iter()
            .filter_map(|entry| match entry {
                BroadcasterPickerEntry::Broadcaster(row)
                    if row.child_of == Some(uncompensated_key) =>
                {
                    Some(row.railgun_address.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["low", "zero"]
    );
    assert!(entries.iter().any(|entry| matches!(
        entry,
        BroadcasterPickerEntry::Group(group)
            if group.key == BroadcasterPickerGroupKey::Status(
                BroadcasterPickerFeeStatus::VeryLowIncentive
            ) && group.fee_tier == BroadcasterPickerTier::OutsideRange
                && group.label == "2 broadcasters outside the allowed range"
    )));
    assert!(matches!(
        entries.last(),
        Some(BroadcasterPickerEntry::Broadcaster(row))
            if row.railgun_address == "unknown"
    ));
}

#[test]
fn uncompensated_singleton_stays_grouped_and_positive_same_rates_stay_direct() {
    let rows = vec![
        picker_row("positive-a", 10, BroadcasterPickerFeeStatus::InRange, 0),
        picker_row("positive-b", 10, BroadcasterPickerFeeStatus::InRange, 1),
        picker_row("positive-c", 10, BroadcasterPickerFeeStatus::InRange, 2),
        picker_row("positive-d", 10, BroadcasterPickerFeeStatus::InRange, 3),
        picker_row("zero", 10, BroadcasterPickerFeeStatus::NoPremium, 4),
    ];
    let entries = grouped(&rows);
    let groups = entries
        .iter()
        .filter_map(|entry| match entry {
            BroadcasterPickerEntry::Group(group) => Some(group),
            BroadcasterPickerEntry::Broadcaster(_) => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(groups.len(), 1);
    assert_eq!(
        groups[0].key,
        BroadcasterPickerGroupKey::Tier(BroadcasterPickerTier::Uncompensated)
    );
    assert_eq!(groups[0].label, "1 broadcaster earning no fee");
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(entry, BroadcasterPickerEntry::Broadcaster(_)))
            .count(),
        4
    );
}

#[test]
fn outside_range_groups_keep_directional_plain_language() {
    let below = vec![
        picker_row("a", 5, BroadcasterPickerFeeStatus::VeryLowIncentive, 0),
        picker_row("b", 7, BroadcasterPickerFeeStatus::VeryLowIncentive, 1),
    ];
    let below_group = picker_group(
        &grouped(&below),
        BroadcasterPickerGroupKey::Status(BroadcasterPickerFeeStatus::VeryLowIncentive),
    );
    assert_eq!(
        below_group.detail,
        "These fees are below the allowed range."
    );
    assert_eq!(below_group.estimated_fee_label, "from 50 TKN");
    assert_eq!(
        below_group.estimated_fee_usd_label.as_deref(),
        Some("from $50")
    );

    let mut below_with_unavailable = below;
    below_with_unavailable[1].estimated_fee_amount = None;
    below_with_unavailable[1].estimated_fee_label = "Retrying...".to_string();
    below_with_unavailable[1].estimated_fee_usd_micro = None;
    below_with_unavailable[1].estimated_fee_usd_label = None;
    let unavailable_below_group = picker_group(
        &grouped(&below_with_unavailable),
        BroadcasterPickerGroupKey::Status(BroadcasterPickerFeeStatus::VeryLowIncentive),
    );
    assert_eq!(unavailable_below_group.estimated_fee_label, "Retrying...");
    assert_eq!(unavailable_below_group.estimated_fee_usd_label, None);

    let mut high = vec![
        picker_row("a", 15, BroadcasterPickerFeeStatus::HighFee, 0),
        picker_row("b", 20, BroadcasterPickerFeeStatus::HighFee, 1),
    ];
    high[1].premium_bps = None;
    let key = BroadcasterPickerGroupKey::Status(BroadcasterPickerFeeStatus::HighFee);
    let mixed_detail = picker_group(&grouped(&high), key).detail;
    assert!(mixed_detail.contains("above the allowed range"));
    assert!(mixed_detail.contains("some comparisons are unavailable"));
    assert!(!mixed_detail.contains('%'));

    high[0].premium_bps = None;
    let unavailable_detail = picker_group(&grouped(&high), key).detail;
    assert!(unavailable_detail.contains("gas-cost comparisons are unavailable"));
    assert!(!unavailable_detail.contains("above the allowed range"));
}

#[test]
fn selected_group_collapse_remains_collapsed_for_same_revision() {
    let mut rows = vec![
        picker_row("a", 8, BroadcasterPickerFeeStatus::NoPremium, 0),
        picker_row("b", 9, BroadcasterPickerFeeStatus::LowIncentive, 1),
        picker_row("c", 10, BroadcasterPickerFeeStatus::NoPremium, 2),
    ];
    rows[2].selected = true;
    let entries = grouped(&rows);
    let group = entries
        .iter()
        .find_map(|entry| match entry {
            BroadcasterPickerEntry::Group(group) => Some(group),
            BroadcasterPickerEntry::Broadcaster(_) => None,
        })
        .expect("uncompensated group");
    assert!(group.expanded);
    assert!(entries.iter().any(|entry| matches!(
        entry,
        BroadcasterPickerEntry::Broadcaster(row)
            if row.railgun_address == "c"
                && row.child_of == Some(group.key)
                && row.selected
    )));

    let mut expanded_groups = BTreeSet::new();
    let mut collapsed_selected_children = BTreeMap::new();
    update_broadcaster_picker_group_expansion(
        &mut expanded_groups,
        &mut collapsed_selected_children,
        group.key,
        true,
        Some("c".to_string()),
        group.revision.clone(),
    );
    let collapsed = project_broadcaster_picker_entries(
        &rows,
        BroadcasterPickerViewMode::Grouped,
        false,
        &expanded_groups,
        &collapsed_selected_children,
    );
    assert_eq!(
        collapsed
            .iter()
            .filter(|entry| matches!(entry, BroadcasterPickerEntry::Broadcaster(_)))
            .count(),
        0
    );
}

#[test]
fn selected_group_reexpands_after_estimate_status_or_membership_revision() {
    let mut rows = vec![
        picker_row("a", 5, BroadcasterPickerFeeStatus::LowIncentive, 0),
        picker_row("b", 7, BroadcasterPickerFeeStatus::LowIncentive, 1),
    ];
    rows[1].selected = true;
    let key = BroadcasterPickerGroupKey::Tier(BroadcasterPickerTier::Uncompensated);
    let initial_entries = grouped(&rows);
    let initial_group = picker_group(&initial_entries, key);
    let mut expanded_groups = BTreeSet::new();
    let mut collapsed_selected_children = BTreeMap::new();
    update_broadcaster_picker_group_expansion(
        &mut expanded_groups,
        &mut collapsed_selected_children,
        key,
        true,
        Some("b".to_string()),
        initial_group.revision.clone(),
    );

    let assert_reexpanded = |revised_rows: &[BroadcasterPickerRow]| {
        let entries = project_broadcaster_picker_entries(
            revised_rows,
            BroadcasterPickerViewMode::Grouped,
            false,
            &expanded_groups,
            &collapsed_selected_children,
        );
        let group = picker_group(&entries, key);
        assert_ne!(group.revision, initial_group.revision);
        assert!(group.expanded);
        assert!(entries.iter().any(|entry| matches!(
            entry,
            BroadcasterPickerEntry::Broadcaster(row)
                if row.railgun_address == "b" && row.selected
        )));
    };

    let mut estimate_changed = rows.clone();
    estimate_changed[0].estimated_fee_amount = Some(U256::from(999));
    estimate_changed[0].estimated_fee_label = "999 TKN".to_string();
    assert_reexpanded(&estimate_changed);

    let mut premium_changed = rows.clone();
    premium_changed[0].premium_bps = Some(-600);
    assert_reexpanded(&premium_changed);

    let mut status_presentation_changed = rows.clone();
    status_presentation_changed[0].fee_status_detail = "Revised status detail".to_string();
    status_presentation_changed[0].fee_warning = Some("Revised warning".to_string());
    assert_reexpanded(&status_presentation_changed);

    let mut assessment_changed = rows.clone();
    assessment_changed[0].fee_status = BroadcasterPickerFeeStatus::NoPremium;
    assert_reexpanded(&assessment_changed);

    let mut favorite_changed = rows.clone();
    favorite_changed[0].favorite = true;
    assert_reexpanded(&favorite_changed);

    let mut membership_changed = rows.clone();
    membership_changed.push(picker_row(
        "c",
        9,
        BroadcasterPickerFeeStatus::LowIncentive,
        2,
    ));
    assert_reexpanded(&membership_changed);

    for row in &mut rows {
        row.fee_status = BroadcasterPickerFeeStatus::VeryLowIncentive;
        row.fee_tier = BroadcasterPickerTier::OutsideRange;
        row.fee_status_detail = "Below allowed range".to_string();
        row.fee_warning = Some("Fee outside allowed range".to_string());
    }
    let revised_key =
        BroadcasterPickerGroupKey::Status(BroadcasterPickerFeeStatus::VeryLowIncentive);
    let status_changed = project_broadcaster_picker_entries(
        &rows,
        BroadcasterPickerViewMode::Grouped,
        false,
        &expanded_groups,
        &collapsed_selected_children,
    );
    let status_changed_group = picker_group(&status_changed, revised_key);
    assert_ne!(status_changed_group.revision, initial_group.revision);
    assert!(status_changed_group.expanded);
    assert!(status_changed.iter().any(|entry| matches!(
        entry,
        BroadcasterPickerEntry::Broadcaster(row)
            if row.railgun_address == "b" && row.selected
    )));
}

#[test]
fn ordinary_group_collapse_does_not_hide_a_later_selected_child() {
    let mut rows = vec![
        picker_row("a", 10, BroadcasterPickerFeeStatus::NoPremium, 0),
        picker_row("b", 11, BroadcasterPickerFeeStatus::LowIncentive, 1),
    ];
    let key = BroadcasterPickerGroupKey::Tier(BroadcasterPickerTier::Uncompensated);
    let group_revision = picker_group(&grouped(&rows), key).revision;
    let mut expanded_groups = BTreeSet::from([key]);
    let mut collapsed_selected_children = BTreeMap::new();

    update_broadcaster_picker_group_expansion(
        &mut expanded_groups,
        &mut collapsed_selected_children,
        key,
        true,
        None,
        group_revision,
    );
    assert!(!expanded_groups.contains(&key));
    assert!(!collapsed_selected_children.contains_key(&key));

    rows[1].selected = true;
    let entries = project_broadcaster_picker_entries(
        &rows,
        BroadcasterPickerViewMode::Grouped,
        false,
        &expanded_groups,
        &collapsed_selected_children,
    );
    assert!(entries.iter().any(|entry| matches!(
        entry,
        BroadcasterPickerEntry::Group(group) if group.key == key && group.expanded
    )));
    assert!(entries.iter().any(|entry| matches!(
        entry,
        BroadcasterPickerEntry::Broadcaster(row) if row.railgun_address == "b" && row.selected
    )));
}

#[test]
fn search_flattens_groups_keeps_tier_order_and_hides_the_divider() {
    let rows = vec![
        picker_row(
            "uncompensated",
            1,
            BroadcasterPickerFeeStatus::LowIncentive,
            0,
        ),
        picker_row("incentivised", 10, BroadcasterPickerFeeStatus::InRange, 1),
    ];
    let entries = project_broadcaster_picker_entries(
        &rows,
        BroadcasterPickerViewMode::Grouped,
        true,
        &BTreeSet::new(),
        &BTreeMap::new(),
    );

    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, BroadcasterPickerEntry::Group(_)))
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(entry, BroadcasterPickerEntry::Broadcaster(_)))
            .count(),
        rows.len()
    );
    assert!(matches!(
        entries.first(),
        Some(BroadcasterPickerEntry::Broadcaster(row))
            if row.railgun_address == "incentivised"
    ));
    assert!(
        !(0..entries.len()).any(|row| broadcaster_picker_section_divider_before(
            &entries,
            BroadcasterPickerViewMode::Grouped,
            row,
        ))
    );
}

#[test]
fn list_mode_uses_global_fee_order_and_preserves_row_state() {
    let mut rows = vec![
        picker_row(
            "outside-cheapest",
            4,
            BroadcasterPickerFeeStatus::HighFee,
            0,
        ),
        picker_row(
            "uncompensated-tie",
            5,
            BroadcasterPickerFeeStatus::LowIncentive,
            1,
        ),
        picker_row("outside-tie", 5, BroadcasterPickerFeeStatus::HighFee, 2),
        picker_row(
            "incentivised-tie",
            5,
            BroadcasterPickerFeeStatus::InRange,
            3,
        ),
    ];
    rows[0].selected = true;
    rows[1].favorite = true;
    let entries = project_broadcaster_picker_entries(
        &rows,
        BroadcasterPickerViewMode::List,
        false,
        &BTreeSet::new(),
        &BTreeMap::new(),
    );
    let listed = entries
        .iter()
        .map(|entry| match entry {
            BroadcasterPickerEntry::Broadcaster(row) => row,
            BroadcasterPickerEntry::Group(_) => {
                panic!("list mode must only contain broadcasters")
            }
        })
        .collect::<Vec<_>>();

    assert_eq!(
        listed
            .iter()
            .map(|row| row.railgun_address.as_str())
            .collect::<Vec<_>>(),
        vec![
            "outside-cheapest",
            "incentivised-tie",
            "uncompensated-tie",
            "outside-tie"
        ]
    );
    assert!(listed[0].selected);
    assert!(listed[0].fee_warning.is_some());
    assert!(!listed[0].show_uncompensated_badge);
    assert!(listed[2].show_uncompensated_badge);
    assert!(listed[2].favorite);
    assert!(
        listed
            .iter()
            .filter(|row| row.favorite)
            .all(|row| row.railgun_address == "uncompensated-tie")
    );
    assert_eq!(
        BroadcasterPickerViewMode::default(),
        BroadcasterPickerViewMode::Grouped
    );
}

#[test]
fn grouped_mode_restores_expansion_and_formats_available_ranges_only() {
    let rows = vec![
        picker_row("a", 5, BroadcasterPickerFeeStatus::LowIncentive, 0),
        picker_row("b", 7, BroadcasterPickerFeeStatus::LowIncentive, 1),
    ];
    let key = BroadcasterPickerGroupKey::Tier(BroadcasterPickerTier::Uncompensated);
    let entries = project_broadcaster_picker_entries(
        &rows,
        BroadcasterPickerViewMode::Grouped,
        false,
        &BTreeSet::from([key]),
        &BTreeMap::new(),
    );
    let group = entries
        .iter()
        .find_map(|entry| match entry {
            BroadcasterPickerEntry::Group(group) => Some(group),
            BroadcasterPickerEntry::Broadcaster(_) => None,
        })
        .expect("expanded status group");
    assert!(group.expanded);
    assert_eq!(group.estimated_fee_label, "from 50 TKN");
    assert_eq!(group.estimated_fee_usd_label.as_deref(), Some("from $50"));

    let mut unavailable = rows.clone();
    for row in &mut unavailable {
        row.estimated_fee_amount = None;
        row.estimated_fee_label = "Estimate unavailable".to_string();
        row.estimated_fee_usd_micro = None;
        row.estimated_fee_usd_label = None;
    }
    assert_eq!(
        group_minimum_estimated_fee_labels(&unavailable),
        ("Estimate unavailable".to_string(), None)
    );
    let mut partially_unavailable = rows.clone();
    partially_unavailable[1].estimated_fee_amount = None;
    partially_unavailable[1].estimated_fee_label = "Retrying...".to_string();
    partially_unavailable[1].estimated_fee_usd_micro = None;
    partially_unavailable[1].estimated_fee_usd_label = None;
    assert_eq!(
        group_minimum_estimated_fee_labels(&partially_unavailable),
        ("Retrying...".to_string(), None)
    );
    let mut no_usd = rows;
    for row in &mut no_usd {
        row.estimated_fee_usd_micro = None;
        row.estimated_fee_usd_label = None;
    }
    assert_eq!(
        group_minimum_estimated_fee_labels(&no_usd),
        ("from 50 TKN".to_string(), None)
    );
}

#[test]
fn grouped_tier_divider_only_precedes_the_uncompensated_summary() {
    let mixed = grouped(&[
        picker_row("incentivised", 10, BroadcasterPickerFeeStatus::InRange, 0),
        picker_row("uncompensated", 5, BroadcasterPickerFeeStatus::NoPremium, 1),
        picker_row(
            "outside",
            20,
            BroadcasterPickerFeeStatus::VeryLowIncentive,
            2,
        ),
    ]);
    assert_eq!(mixed.len(), 3);
    assert!(!broadcaster_picker_section_divider_before(
        &mixed,
        BroadcasterPickerViewMode::Grouped,
        0,
    ));
    assert!(broadcaster_picker_section_divider_before(
        &mixed,
        BroadcasterPickerViewMode::Grouped,
        1,
    ));
    assert!(!broadcaster_picker_section_divider_before(
        &mixed,
        BroadcasterPickerViewMode::List,
        1,
    ));
    assert!(!broadcaster_picker_section_divider_before(
        &mixed,
        BroadcasterPickerViewMode::Grouped,
        2,
    ));

    let incentivised_only = grouped(&[picker_row(
        "positive",
        20,
        BroadcasterPickerFeeStatus::InRange,
        0,
    )]);
    let uncompensated_only = grouped(&[picker_row(
        "zero",
        10,
        BroadcasterPickerFeeStatus::NoPremium,
        0,
    )]);
    assert!(!(0..incentivised_only.len()).any(|row| {
        broadcaster_picker_section_divider_before(
            &incentivised_only,
            BroadcasterPickerViewMode::Grouped,
            row,
        )
    }));
    assert!(!(0..uncompensated_only.len()).any(|row| {
        broadcaster_picker_section_divider_before(
            &uncompensated_only,
            BroadcasterPickerViewMode::Grouped,
            row,
        )
    }));
}

#[test]
fn status_tooltip_width_is_capped_and_yields_to_available_viewport_width() {
    assert_eq!(
        broadcaster_picker_status_tooltip_width(px(1_000.0), px(16.0)),
        Some(px(320.0))
    );
    assert_eq!(
        broadcaster_picker_status_tooltip_width(px(300.0), px(16.0)),
        Some(px(258.0))
    );
    assert_eq!(
        broadcaster_picker_status_tooltip_width(px(42.0), px(16.0)),
        None
    );
}

#[test]
fn status_tooltip_revision_changes_with_live_status_content() {
    let current = broadcaster_picker_status_tooltip_revision(
        BroadcasterPickerTier::Incentivised,
        "Current detail",
    );
    assert_eq!(
        current,
        broadcaster_picker_status_tooltip_revision(
            BroadcasterPickerTier::Incentivised,
            "Current detail"
        )
    );
    assert_ne!(
        current,
        broadcaster_picker_status_tooltip_revision(
            BroadcasterPickerTier::Incentivised,
            "Updated detail"
        )
    );
    assert_ne!(
        current,
        broadcaster_picker_status_tooltip_revision(
            BroadcasterPickerTier::OutsideRange,
            "Current detail"
        )
    );
}
