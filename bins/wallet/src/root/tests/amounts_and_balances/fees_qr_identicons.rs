use super::*;

#[test]
fn eip1559_gas_fee_helpers_parse_and_format_gwei() {
    assert_eq!(parse_gwei_to_wei("1"), Ok(1_000_000_000));
    assert_eq!(parse_gwei_to_wei("1.25"), Ok(1_250_000_000));
    assert_eq!(parse_gwei_to_wei("0.000000001"), Ok(1));
    assert!(parse_gwei_to_wei("0.0000000001").is_err());
    assert_eq!(format_gwei(1_250_000_000), "1.25");
    assert_eq!(format_gwei(1), "0.000000001");
}

#[test]
fn eip1559_custom_gas_fee_validation_rejects_invalid_caps() {
    assert!(validate_custom_gas_fee(1, 0).is_ok());
    assert!(validate_custom_gas_fee(1, 1).is_ok());
    assert!(validate_custom_gas_fee(0, 0).is_err());
    assert!(validate_custom_gas_fee(1, 2).is_err());
}

#[test]
fn public_address_qr_payload_is_plain_address() {
    let address = Address::from([0xab; 20]);
    let payload = public_address_qr_payload(address);

    assert_eq!(payload, format!("{address:#x}"));
    assert!(!payload.starts_with("ethereum:"));
}

#[test]
fn public_address_qr_payload_fits_qr_with_quiet_zone() {
    let address = Address::from([0x42; 20]);
    let payload = public_address_qr_payload(address);
    let qr = qrcodegen::QrCode::encode_text(&payload, qrcodegen::QrCodeEcc::Medium)
        .expect("public address should fit in QR code");
    let module_range = public_address_qr_module_range(qr.size());

    assert!(qr.size() > 0);
    assert_eq!(PUBLIC_ADDRESS_QR_QUIET_ZONE_MODULES, 4);
    assert_eq!(
        module_range.clone().count(),
        usize::try_from(qr.size() + 8).unwrap()
    );
    assert!(module_range.contains(&-PUBLIC_ADDRESS_QR_QUIET_ZONE_MODULES));
    assert!(module_range.contains(&qr.size()));
}

#[test]
fn public_account_identicon_pattern_is_deterministic_and_symmetric() {
    let address = Address::from([0x42; 20]);
    let pattern = public_account_identicon_pattern(&address);

    assert_eq!(pattern, public_account_identicon_pattern(&address));
    assert!(pattern.iter().any(|active| *active));
    for row in pattern.as_chunks::<PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE>().0 {
        assert_eq!(row[0], row[4]);
        assert_eq!(row[1], row[3]);
    }
}

#[test]
fn public_account_identicon_differs_for_different_addresses() {
    let first = Address::from([0x11; 20]);
    let second = Address::from([0x22; 20]);

    assert_ne!(
        public_account_identicon_pattern(&first),
        public_account_identicon_pattern(&second),
    );
    assert_ne!(
        public_account_identicon_color(&first),
        public_account_identicon_color(&second),
    );
}

#[test]
fn public_account_identicon_zero_address_is_not_blank() {
    let pattern = public_account_identicon_pattern(&Address::from([0; 20]));
    let active_count = pattern.iter().filter(|active| **active).count();

    assert_eq!(active_count, 1);
    assert!(pattern[PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT / 2]);
}
