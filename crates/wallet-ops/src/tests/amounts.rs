use super::helpers::*;

#[test]
fn parse_unshield_amount_scales_known_token_decimals() {
    assert_eq!(
        parse_unshield_amount("1.23", Some(6)).expect("parsed amount"),
        uint!(1_230_000_U256)
    );
    assert_eq!(
        parse_unshield_amount(".5", Some(18)).expect("parsed amount"),
        uint!(5_U256) * uint!(10_U256).pow(uint!(17_U256))
    );
}

#[test]
fn parse_unshield_amount_rejects_too_much_precision() {
    assert!(parse_unshield_amount("1.2345678", Some(6)).is_err());
}

#[test]
fn token_amount_parsing_rejects_overflow_instead_of_changing_the_payment() {
    let maximum = U256::MAX.to_string();
    assert_eq!(parse_send_amount(&maximum, None).unwrap(), U256::MAX);
    assert!(parse_send_amount(&maximum, Some(18)).is_err());
    // The whole part fits after scaling; adding the fractional part overflows.
    let beyond = format!("{}.6", U256::MAX / U256::from(10));
    assert!(parse_send_amount(&beyond, Some(1)).is_err());
    assert!(parse_send_amount("1", Some(255)).is_err());
}

#[test]
fn parse_unshield_amount_requires_raw_units_for_unknown_tokens() {
    assert_eq!(
        parse_unshield_amount("123", None).expect("parsed raw amount"),
        uint!(123_U256)
    );
    assert!(parse_unshield_amount("1.23", None).is_err());
}

#[test]
fn parse_send_amount_reuses_token_aware_amount_parsing() {
    assert_eq!(
        parse_send_amount("1.23", Some(6)).expect("parsed amount"),
        uint!(1_230_000_U256)
    );
    assert_eq!(
        parse_send_amount("123", None).expect("parsed raw amount"),
        uint!(123_U256)
    );
    assert!(parse_send_amount("1.23", None).is_err());
}

#[test]
fn parse_railgun_recipient_accepts_valid_0zk_address() {
    let wallet = WalletKeys::from_mnemonic(
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        0,
    )
    .expect("derive wallet");
    let address = wallet
        .viewing
        .derive_address(None)
        .expect("derive all-chain address")
        .to_string();

    let recipient = parse_railgun_recipient(&address).expect("valid 0zk recipient");

    assert_eq!(
        recipient.master_public_key,
        wallet.viewing.master_public_key
    );
    assert_eq!(
        recipient.viewing_public_key,
        wallet.viewing.viewing_public_key
    );
}

#[test]
fn parse_railgun_recipient_rejects_invalid_address() {
    assert!(parse_railgun_recipient("0x0000000000000000000000000000000000000000").is_err());
    assert!(parse_railgun_recipient("").is_err());
}

#[test]
fn wrapped_native_detection_matches_supported_chains() {
    let weth = wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
    assert!(is_wrapped_native_token(1, weth));
    assert!(!is_wrapped_native_token(1, address(0x11)));
    assert!(wrapped_native_token_for_chain(999_999).is_none());
}
