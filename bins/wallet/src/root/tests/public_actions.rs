use std::sync::Arc;

use alloy::primitives::{Bytes, U256, address, keccak256};
use wallet_ops::vault::PublicAccountSource;
use wallet_ops::{
    NativeShieldPayer, PublicActionGasFeeMode, PublicActionGasFeeQuote,
    PublicActionGasFeeSelection, PublicAssetId, PublicShieldTransactionProfile,
    PublicTransactionIntent,
};

use super::*;
use crate::root::public_action::{
    NativeShieldKey, NativeShieldPreparationState, bind_native_shield_operation,
    native_shield_payer,
};

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
            PublicAssetId::Erc20(address!("0x3333333333333333333333333333333333333333")),
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
            PublicAssetId::Erc20(address!("0x3333333333333333333333333333333333333333")),
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
            PublicAssetId::Erc20(address!("0x3333333333333333333333333333333333333333")),
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

#[test]
fn stale_native_shield_preparation_cannot_establish_readiness() {
    let key_in = |profile, amount: u64| NativeShieldKey {
        wallet_id: Some(Arc::from("wallet")),
        chain_id: 56,
        public_account_uuid: Arc::from("account"),
        profile,
        amount: U256::from(amount),
    };
    let key = |amount| key_in(PublicShieldTransactionProfile::Railoxide, amount);
    let mut preparation = NativeShieldPreparationState::<u64>::default();
    let earlier = preparation.begin();
    let current = preparation.begin();
    assert!(!preparation.complete(earlier, key(1), Some(&key(1)), Ok(1)));
    assert!(preparation.prepared(Some(&key(1))).is_none());
    // The latest request still cannot serve inputs edited after it started.
    assert!(!preparation.complete(current, key(2), Some(&key(3)), Ok(2)));
    assert!(preparation.complete(current, key(2), Some(&key(2)), Ok(2)));
    assert_eq!(preparation.prepared(Some(&key(2))), Some(&2));
    assert!(preparation.prepared(Some(&key(3))).is_none());
    // Toggling Mimic Railway changes the gas buffer and fee bounds, so it prepares again.
    let mimic = key_in(PublicShieldTransactionProfile::Railway, 2);
    assert!(preparation.prepared(Some(&mimic)).is_none());
    // Preparing again in the other profile is the same action, with the same stealth account.
    let mut bound = None;
    let operation = bind_native_shield_operation(&mut bound, key(2)).unwrap();
    assert_eq!(
        bind_native_shield_operation(&mut bound, mimic).unwrap(),
        operation
    );
    assert_ne!(
        bind_native_shield_operation(&mut bound, key(3)).unwrap(),
        operation
    );
    preparation.invalidate();
    assert!(preparation.prepared(Some(&key(2))).is_none());
}

#[test]
fn native_shield_payer_follows_the_account_source() {
    let executor = serde_json::from_value(serde_json::json!({
        "chain_id": 1,
        "index": 5,
        "operation": wallet_ops::vault::ExecutorOperationId::random().unwrap(),
    }))
    .unwrap();
    for (source, wallet_is_hardware, payer) in [
        (
            PublicAccountSource::HardwareDerived,
            true,
            NativeShieldPayer::Hardware,
        ),
        // A Ledger or Trezor wallet approves its stealth account with a software payer only.
        (
            PublicAccountSource::Imported,
            true,
            NativeShieldPayer::Software,
        ),
        (
            PublicAccountSource::ExecutorDerived(executor),
            true,
            NativeShieldPayer::HardwareWallet,
        ),
        (
            PublicAccountSource::ExecutorDerived(executor),
            false,
            NativeShieldPayer::Software,
        ),
    ] {
        assert_eq!(native_shield_payer(source, wallet_is_hardware), payer);
    }
}
