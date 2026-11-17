use super::*;
use alloy::primitives::address;
use alloy::uint;
use std::time::SystemTime;

fn row(chain_id: u64, broadcaster: &str, token: Address, identifier: Option<&str>) -> FeeRow {
    FeeRow {
        chain_id,
        railgun_address: Arc::from(broadcaster),
        token_address: token,
        fee: uint!(0_U256),
        fees_id: Arc::from("fid"),
        fee_expiration: SystemTime::now(),
        available_wallets: 1,
        version: Arc::from("8.2.3"),
        relay_adapt: Some(address!("0000000000000000000000000000000000000002")),
        relay_adapt_7702: None,
        required_poi_list_keys: Vec::new(),
        identifier: identifier.map(Arc::from),
        last_seen: SystemTime::now(),
        reliability: 1.0,
    }
}

fn row_with_fee(fee: U256) -> FeeRow {
    let mut row = row(
        1,
        "0zkaaa",
        address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        None,
    );
    row.fee = fee;
    row
}

fn fixed_anchor_lookup(anchor: Option<U256>) -> FeeAnchorLookup {
    Arc::new(move |_chain_id, _token| anchor)
}

#[test]
fn fee_bonus_bps_calculates_premium_and_discount() {
    assert_eq!(
        fee_bonus_bps_from_anchor(uint!(1_500_U256), uint!(1_000_U256)),
        Some(5_000)
    );
    assert_eq!(
        fee_bonus_bps_from_anchor(uint!(900_U256), uint!(1_000_U256)),
        Some(-1_000)
    );
}

#[test]
fn fee_bonus_label_formats_signed_percent() {
    assert_eq!(format_bonus_bps(5_250), "+52.5%");
    assert_eq!(format_bonus_bps(-1_000), "-10.0%");
}

#[test]
fn fee_bonus_sort_keeps_missing_anchor_last() {
    assert_eq!(
        compare_optional_bonus(Some(100), None, ColumnSort::Ascending),
        Ordering::Less
    );
    assert_eq!(
        compare_optional_bonus(None, Some(100), ColumnSort::Descending),
        Ordering::Greater
    );
    assert_eq!(
        compare_optional_bonus(Some(100), Some(200), ColumnSort::Ascending),
        Ordering::Less
    );
    assert_eq!(
        compare_optional_bonus(Some(100), Some(200), ColumnSort::Descending),
        Ordering::Greater
    );
}

#[test]
fn raw_fee_label_remains_token_scaled() {
    let row = row_with_fee(uint!(1_234_000_U256));

    assert_eq!(raw_fee_label(&row), "1.23");
}

#[test]
fn fee_bonus_uses_anchor_lookup_and_allows_cache_miss() {
    let row = row_with_fee(uint!(1_500_U256));

    assert_eq!(
        fee_bonus_bps(&row, &fixed_anchor_lookup(Some(uint!(1_000_U256)))),
        Some(5_000)
    );
    assert_eq!(fee_bonus_bps(&row, &fixed_anchor_lookup(None)), None);
}

#[test]
fn chain_filter_one_narrows_rows_to_that_chain() {
    let t = address!("0x0000000000000000000000000000000000000001");
    let r1 = row(1, "0zkaaa", t, None);
    let r137 = row(137, "0zkbbb", t, None);
    assert!(matches_chain(&r1, FeesFilter::One(1)));
    assert!(!matches_chain(&r137, FeesFilter::One(1)));
    assert!(matches_chain(&r1, FeesFilter::All));
    assert!(matches_chain(&r137, FeesFilter::All));
}

#[test]
fn broadcaster_query_matches_identifier_and_address_case_insensitive() {
    let t = address!("0x0000000000000000000000000000000000000001");
    let r = row(1, "0zkABCdef", t, Some("Alice"));
    // Empty query always matches.
    assert!(matches_broadcaster(&r, ""));
    // The caller is expected to pre-lowercase the query (that's what the
    // subscription in root.rs does); predicate does per-row lowercasing.
    assert!(matches_broadcaster(&r, "abcdef"));
    assert!(matches_broadcaster(&r, "ali"));
    assert!(!matches_broadcaster(&r, "zzz"));
    // A broadcaster without an identifier still matches by address.
    let r_no_id = row(1, "0zkABCdef", t, None);
    assert!(matches_broadcaster(&r_no_id, "abcdef"));
    assert!(!matches_broadcaster(&r_no_id, "ali"));
}

#[test]
fn token_filter_items_compare_value_and_chain_display_scope() {
    let token = address!("0x0000000000000000000000000000000000000001");

    assert_eq!(
        FeesTokenFilterItem::token(1, token, false),
        FeesTokenFilterItem::token(1, token, false)
    );
    assert_ne!(
        FeesTokenFilterItem::token(1, token, false),
        FeesTokenFilterItem::token(1, token, true)
    );
}

#[test]
fn cascade_reset_drops_token_filter_when_chain_changes() {
    let t = address!("0x0000000000000000000000000000000000000001");
    let token_on_chain_1 = FeesFilter::One((1u64, t));

    // Switching to a different chain drops the token filter.
    assert_eq!(
        cascade_reset_token(FeesFilter::One(137), token_on_chain_1),
        FeesFilter::All
    );
    // Staying on the same chain preserves it.
    assert_eq!(
        cascade_reset_token(FeesFilter::One(1), token_on_chain_1),
        token_on_chain_1
    );
    // Removing the chain filter preserves the token filter.
    assert_eq!(
        cascade_reset_token(FeesFilter::All, token_on_chain_1),
        token_on_chain_1
    );
    // When no token filter is set, any chain change is a no-op.
    assert_eq!(
        cascade_reset_token(FeesFilter::One(137), FeesFilter::All),
        FeesFilter::All
    );
}
