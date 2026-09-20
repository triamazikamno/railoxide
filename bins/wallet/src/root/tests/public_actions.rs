use alloy::primitives::{Bytes, U256, address, keccak256};
use wallet_ops::vault::PublicAccountSource;
use wallet_ops::{
    PublicActionGasFeeMode, PublicActionGasFeeQuote, PublicActionGasFeeSelection, PublicAssetId,
    PublicShieldTransactionProfile, PublicTransactionIntent,
};

use super::*;

#[test]
fn advanced_public_send_parser_accepts_calls_creation_and_optional_prefix() {
    let to = address!("0x2222222222222222222222222222222222222222");
    let call = parse_advanced_public_send_intent(
        PublicSendKind::ContractCall,
        &to.to_checksum(None),
        "1.5",
        "0x12345678aa",
        Some(18),
    )
    .expect("advanced call");
    assert_eq!(
        call,
        PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::from(1_500_000_000_000_000_000_u64),
            data: Bytes::from_static(&[0x12, 0x34, 0x56, 0x78, 0xaa]),
        }
    );

    let creation =
        parse_advanced_public_send_intent(PublicSendKind::Deploy, "", "", "60006000", Some(18))
            .expect("advanced creation");
    assert!(matches!(
        creation,
        PublicTransactionIntent::Raw {
            to: None,
            value: U256::ZERO,
            ..
        }
    ));

    let odd = parse_advanced_public_send_intent(
        PublicSendKind::ContractCall,
        &to.to_checksum(None),
        "",
        "0x123",
        Some(18),
    )
    .expect_err("odd hex must fail");
    assert_eq!(odd.field, AdvancedPublicSendField::Data);
    let invalid_to = parse_advanced_public_send_intent(
        PublicSendKind::ContractCall,
        "not-an-address",
        "1",
        "",
        Some(18),
    )
    .expect_err("invalid destination must fail");
    assert_eq!(invalid_to.field, AdvancedPublicSendField::Destination);

    let missing_to =
        parse_advanced_public_send_intent(PublicSendKind::ContractCall, "", "", "", Some(18))
            .expect_err("contract call destination must be explicit");
    assert_eq!(missing_to.field, AdvancedPublicSendField::Destination);

    let missing_init_code = parse_advanced_public_send_intent(
        PublicSendKind::Deploy,
        &to.to_checksum(None),
        "",
        "",
        Some(18),
    )
    .expect_err("deploy init code must be explicit");
    assert_eq!(missing_init_code.field, AdvancedPublicSendField::Data);
}

#[test]
fn advanced_public_send_metadata_and_count_formatting_are_deterministic() {
    let to = address!("0x2222222222222222222222222222222222222222");
    let data = Bytes::from_static(&[0x12, 0x34, 0x56, 0x78, 0xaa]);
    let metadata = advanced_public_send_review_metadata(&PublicTransactionIntent::Raw {
        to: Some(to),
        value: U256::from(7_u64),
        data: data.clone(),
    })
    .expect("advanced metadata");

    assert_eq!(metadata.action_type, "Contract call");
    assert_eq!(metadata.destination, to.to_checksum(None));
    assert_eq!(metadata.data_length, 5);
    assert_eq!(metadata.selector.as_deref(), Some("0x12345678"));
    assert_eq!(
        metadata.data_hash,
        alloy::hex::encode_prefixed(keccak256(&data))
    );
    assert_eq!(metadata.full_data, "0x12345678aa");
    assert_eq!(format_advanced_data_length(1), "1 byte");
    assert_eq!(format_advanced_data_length(2), "2 bytes");
    assert_eq!(format_gas_limit(999), "999");
    assert_eq!(format_gas_limit(1_000), "1,000");
}

#[test]
fn advanced_public_send_hardware_warning_is_added() {
    let software_warnings = advanced_public_send_warnings(PublicAccountSource::Derived);
    assert_eq!(software_warnings.len(), 1);
    assert!(software_warnings[0].contains("Arbitrary transaction data"));

    let hardware_warnings = advanced_public_send_warnings(PublicAccountSource::HardwareDerived);
    assert_eq!(hardware_warnings.len(), 2);
    assert!(hardware_warnings[1].contains("blind signing"));
}

#[test]
fn railway_bnb_auto_authorization_binds_rpc_gas_price_as_legacy_fee() {
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: None,
        rpc_gas_price: 7,
        current_base_fee_per_gas: Some(5),
        suggested_max_fee_per_gas: 12,
        suggested_max_priority_fee_per_gas: 2,
    };
    assert_eq!(
        authorized_public_action_gas_fee_selection(
            PublicActionGasFeeSelection::Auto,
            Some(quote),
            PublicShieldTransactionProfile::Railway,
            56,
        )
        .expect("Railway BNB authorization fee"),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 7,
            max_priority_fee_per_gas: 0,
        }
    );
    assert_eq!(
        authorized_public_action_gas_fee_selection(
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 11,
                max_priority_fee_per_gas: 3,
            },
            Some(quote),
            PublicShieldTransactionProfile::Railway,
            56,
        )
        .expect("Railway BNB custom authorization fee"),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 11,
            max_priority_fee_per_gas: 3,
        }
    );
    assert_eq!(
        authorized_public_action_gas_fee_selection(
            PublicActionGasFeeSelection::Auto,
            Some(quote),
            PublicShieldTransactionProfile::Railway,
            1,
        )
        .expect("Railway type-2 authorization fee"),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 12,
            max_priority_fee_per_gas: 2,
        }
    );
}

#[test]
fn railway_authorization_ceiling_applies_only_to_erc20_shield_auto() {
    let token = address!("0x3333333333333333333333333333333333333333");
    assert!(!public_action_uses_railway_authorization_ceiling(
        PublicActionMode::Shield,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Native,
        PublicActionGasFeeMode::Auto,
    ));
    assert!(public_action_uses_railway_authorization_ceiling(
        PublicActionMode::Shield,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Erc20(token),
        PublicActionGasFeeMode::Auto,
    ));
    assert!(!public_action_uses_railway_authorization_ceiling(
        PublicActionMode::Shield,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Erc20(token),
        PublicActionGasFeeMode::Custom,
    ));
    assert!(!public_action_uses_railway_authorization_ceiling(
        PublicActionMode::Send,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Erc20(token),
        PublicActionGasFeeMode::Auto,
    ));
    assert!(!public_action_uses_railway_authorization_ceiling(
        PublicActionMode::Shield,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Erc20(token),
        PublicActionGasFeeMode::Auto,
    ));
}

#[test]
fn explicit_review_summaries_cannot_use_cached_password() {
    let ordinary = SpendAuthorizationSummary::new("Send", "Review", Vec::new());
    assert!(spend_authorization_can_use_cached_password(&ordinary));

    let advanced = ordinary.requiring_explicit_review();
    assert!(!spend_authorization_can_use_cached_password(&advanced));
}
