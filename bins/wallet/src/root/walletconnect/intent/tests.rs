use super::*;
use crate::root::dapp_request::{DappRequestBinding, DappSessionIdentity};
use serde_json::json;
use std::collections::BTreeMap;
use wallet_ops::settings::EffectiveTokenInfo;
use wallet_ops::vault::{
    PublicAccountScope, PublicAccountSource, PublicAccountStatus, WalletConnectPeerMetadata,
};

#[test]
fn simulation_revert_becomes_distinct_risk_but_unavailable_does_not() {
    let risk = walletconnect_simulation_risk(
        &WalletConnectFeeStatus::WouldRevert,
        Some("execution reverted"),
    );
    assert!(matches!(
        risk,
        Some(WalletConnectRisk::WouldRevert { ref reason }) if reason == "execution reverted"
    ));
    assert!(
        walletconnect_simulation_risk(
            &WalletConnectFeeStatus::UnavailableFailed,
            Some("provider unavailable"),
        )
        .is_none()
    );
}

#[test]
fn approval_uses_spender_only_and_keeps_token_contract_out_of_primary_parties() {
    let account = Address::from([0x11; 20]);
    let token = Address::from([0x22; 20]);
    let spender = Address::from([0x33; 20]);
    let chain = chain(None);
    let registry = registry(token);
    let rates = TokenAnchorRateCache::new();
    let request = request_with(
        WalletConnectParsedRequest::EthSendTransaction {
            transaction: transaction(account, Some(token), U256::ZERO),
        },
        Some(WalletConnectDecodedTransaction {
            target: Some(token),
            native_value: U256::ZERO,
            kind: WalletConnectDecodedCallKind::Erc20Approve {
                spender,
                amount: U256::from(1),
            },
        }),
    );
    let intent = build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));
    assert_eq!(intent.action, WalletConnectIntentAction::Approve);
    assert_eq!(intent.parties.len(), 1);
    assert_eq!(intent.parties[0].role, WalletConnectPartyRole::Spender);
    assert!(!walletconnect_should_render_token_contract(intent.action));
    let summary =
        super::super::requests::walletconnect_request_authorization_summary(&request, &intent);
    let authorization = summary
        .rows_for_test()
        .into_iter()
        .find(|(label, _)| label == "Intent")
        .expect("intent authorization row")
        .1;
    assert!(authorization.starts_with("Allow spending up to "));
    assert!(authorization.contains("USDC"));
    assert!(
        summary
            .rows_for_test()
            .contains(&("Spender".to_owned(), spender.to_checksum(None),))
    );
    assert!(
        !authorization.contains("Spender can withdraw up to"),
        "approval effect should remain on the review card, not the final summary"
    );
}

#[test]
fn authorization_summary_promotes_walletconnect_risks_to_warnings() {
    let account = Address::from([0x11; 20]);
    let token = Address::from([0x22; 20]);
    let spender = Address::from([0x33; 20]);
    let chain = chain(None);
    let registry = registry(token);
    let rates = TokenAnchorRateCache::new();
    let request = request_with(
        WalletConnectParsedRequest::EthSendTransaction {
            transaction: transaction(account, Some(token), U256::ZERO),
        },
        Some(WalletConnectDecodedTransaction {
            target: Some(token),
            native_value: U256::ZERO,
            kind: WalletConnectDecodedCallKind::Erc20Approve {
                spender,
                amount: U256::MAX,
            },
        }),
    );
    let intent = build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));
    let summary =
        super::super::requests::walletconnect_request_authorization_summary(&request, &intent);

    assert!(
        summary
            .warnings_for_test()
            .iter()
            .any(|warning| warning.starts_with("Unlimited allowance:"))
    );
}

#[test]
fn approval_effect_distinguishes_known_and_unresolved_allowances() {
    let known = WalletConnectAmount::KnownToken {
        token: Address::ZERO,
        raw: U256::from(5),
        decimals: 6,
        symbol: "USDC".to_owned(),
        display: "0.000005 USDC".to_owned(),
        usd: None,
    };
    assert_eq!(
        walletconnect_approval_effect(WalletConnectIntentAction::Approve, &known),
        Some("Spender can withdraw up to 0.000005 USDC across one or more transactions until the allowance is used or revoked.".to_owned())
    );
    let unlimited = WalletConnectAmount::Unlimited {
        token: Address::ZERO,
        raw: U256::MAX,
        decimals: Some(6),
        symbol: Some("USDC".to_owned()),
        display: "Unlimited USDC".to_owned(),
        unrecognised_contract: false,
    };
    assert_eq!(
        walletconnect_approval_effect(WalletConnectIntentAction::Approve, &unlimited),
        Some("No fixed limit in USDC until the allowance is revoked.".to_owned())
    );
    let unresolved = WalletConnectAmount::RawToken {
        token: Address::ZERO,
        raw: U256::from(123),
        display: "123 raw token units".to_owned(),
        unrecognised_contract: true,
        decimals_unknown: true,
    };
    let unresolved_effect =
        walletconnect_approval_effect(WalletConnectIntentAction::Approve, &unresolved)
            .expect("unresolved finite effect");
    assert!(unresolved_effect.contains("123 raw token units"));
    assert!(unresolved_effect.contains("ERC-20-shaped"));
    assert!(unresolved_effect.contains("unknown contract"));
    assert!(unresolved_effect.contains("not verified"));
    let unresolved_unlimited = WalletConnectAmount::Unlimited {
        token: Address::ZERO,
        raw: U256::MAX,
        decimals: None,
        symbol: None,
        display: "Unlimited raw token units".to_owned(),
        unrecognised_contract: true,
    };
    let unresolved_unlimited_effect =
        walletconnect_approval_effect(WalletConnectIntentAction::Approve, &unresolved_unlimited)
            .expect("unresolved unlimited effect");
    assert!(unresolved_unlimited_effect.contains("No fixed limit"));
    assert!(unresolved_unlimited_effect.contains("unknown contract"));
    assert!(
        walletconnect_approval_effect(WalletConnectIntentAction::TokenTransfer, &known,).is_none()
    );
}

#[test]
fn authorization_summary_merges_projected_site_and_dapp_name() {
    let account = Address::from([0x11; 20]);
    let chain = chain(None);
    let registry = EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let rates = TokenAnchorRateCache::new();
    let request = request_with(
        WalletConnectParsedRequest::EthSignTypedDataV4 {
            account,
            typed_data: serde_json::json!({}),
            domain_chain_id: Some(U256::from(1)),
        },
        None,
    );
    let intent = build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));
    let summary =
        super::super::requests::walletconnect_request_authorization_summary(&request, &intent);
    let rows = summary.rows_for_test();
    assert!(rows.contains(&("Requested by".to_owned(), "app.aave.com (Aave)".to_owned())));
    assert!(!rows.iter().any(|(label, _)| label == "Site"));
    assert!(!rows.iter().any(|(label, _)| label == "Dapp"));
    assert!(
        !rows
            .iter()
            .any(|(label, _)| label == "Maximum network cost")
    );
}

#[test]
fn walletconnect_authorization_summary_includes_reviewed_maximum_cost() {
    let account = Address::from([0x11; 20]);
    let target = Address::from([0x22; 20]);
    let chain = chain(None);
    let registry = EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let rates = TokenAnchorRateCache::new();
    let request = request_with(
        WalletConnectParsedRequest::EthSendTransaction {
            transaction: transaction(account, Some(target), U256::from(1)),
        },
        Some(WalletConnectDecodedTransaction {
            target: Some(target),
            native_value: U256::from(1),
            kind: WalletConnectDecodedCallKind::NativeTransfer,
        }),
    );
    let intent = build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));
    let mut reviewed = super::super::fee::WalletConnectReviewedFeeProjection::unresolved(
        request.key.as_str(),
        request.review_token,
    );
    reviewed.maximum_gas_cost = Some(U256::from(123_456_u64));
    let rows = super::super::requests::walletconnect_request_authorization_summary_with_fee(
        &request,
        &intent,
        Some(&reviewed),
    )
    .rows_for_test();
    assert!(
        rows.iter()
            .any(|(label, value)| label == "Maximum network cost" && !value.is_empty())
    );
    assert!(rows.contains(&("To".to_owned(), target.to_checksum(None))));
}

#[test]
fn critical_party_address_projection_is_single_and_responsive() {
    let address = Address::from_slice(
        &alloy::hex::decode("1234567890abcdef1234567890abcdef12345678").expect("address"),
    );
    let full = address.to_checksum(None);
    let short = walletconnect_checksummed_address_label(&address);
    assert_eq!(
        walletconnect_party_address_label(
            WalletConnectPartyRole::Spender,
            &address,
            WALLETCONNECT_CRITICAL_PARTY_FULL_ADDRESS_MIN_WIDTH,
        ),
        full
    );
    assert_eq!(
        walletconnect_party_address_label(WalletConnectPartyRole::Recipient, &address, px(499.0),),
        short
    );
    assert_eq!(
        walletconnect_party_address_label(WalletConnectPartyRole::Caller, &address, px(620.0),),
        short
    );
}

#[test]
fn peer_provenance_normalizes_host_and_sanitizes_capped_name() {
    let metadata = WalletConnectPeerMetadata {
        name: format!("  Evil\u{202e} dapp\n{}", "x".repeat(100)),
        description: String::new(),
        url: "HTTPS://Example.COM:443/path".to_owned(),
        icons: Vec::new(),
    };
    let projection = walletconnect_peer_provenance(&metadata.name, &metadata.url);
    assert_eq!(projection.site, "example.com");
    let dapp_name = projection.dapp_name.expect("sanitized dapp name");
    assert!(!dapp_name.contains('\u{202e}'));
    assert_eq!(dapp_name.chars().count(), 80);

    let empty = WalletConnectPeerMetadata {
        name: "\u{200f}\u{0000} \t".to_owned(),
        description: String::new(),
        url: "not a url".to_owned(),
        icons: Vec::new(),
    };
    let projection = walletconnect_peer_provenance(&empty.name, &empty.url);
    assert_eq!(projection.site, "Unknown peer site");
    assert_eq!(projection.dapp_name, None);
}

use wallet_ops::{WalletConnectEvmTransaction, WalletConnectPendingRequest};

fn chain(wrapped_native_token: Option<&str>) -> EffectiveChainConfig {
    let mut chain = wallet_ops::settings::build_effective_chain_configs(
        &wallet_ops::settings::WalletSettings::default(),
    )
    .unwrap()
    .get(1)
    .cloned()
    .unwrap();
    chain.wrapped_native_token = wrapped_native_token.map(|value| value.parse().unwrap());
    chain
}

fn context<'a>(
    chain: &'a EffectiveChainConfig,
    registry: &'a EffectiveTokenRegistry,
    rates: &'a TokenAnchorRateCache,
    accounts: &'a [PublicAccountMetadata],
    address_book: &'a [PublicAddressBookEntry],
) -> WalletConnectIntentContext<'a> {
    WalletConnectIntentContext {
        chain,
        selected_chain_id: chain.chain_id,
        token_registry: registry,
        anchor_rates: rates,
        public_accounts: accounts,
        public_address_book: address_book,
    }
}

fn request_with(
    parsed: WalletConnectParsedRequest,
    decoded_transaction: Option<WalletConnectDecodedTransaction>,
) -> WalletConnectRequestUi {
    let account = Address::from([0x11; 20]);
    let method = parsed.method();
    WalletConnectRequestUi {
        request_control: None,
        rpc_reads: None,
        operation: None,
        key: "session-topic:7".to_owned(),
        review_token: 1,
        binding: DappRequestBinding {
            account: Some(super::super::DappRequestAccount {
                public_account_uuid: "public-account".to_owned(),
                public_account_scope: PublicAccountScope::Global,
                owning_private_wallet_uuid: None,
            }),
            peer_name: "Aave".to_owned(),
            peer_url: "https://app.aave.com".to_owned(),
        },
        session_identity: DappSessionIdentity::walletconnect("session-uuid".to_owned()),
        parsed,
        item: WalletConnectPendingRequest {
            id: 7,
            topic: "session-topic".to_owned(),
            dapp_name: "Aave".to_owned(),
            chain_id: "eip155:1".to_owned(),
            method,
            account: Some(account),
            decoded_transaction,
            raw_details: json!({}),
            expiry_timestamp: Some(1_700_000_300),
        },
        account_source: Some(PublicAccountSource::Imported),
    }
}

#[test]
fn chain_switch_intent_shows_destination_and_network_transition() {
    let registry = EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let rates = TokenAnchorRateCache::default();
    for (website_chain_id, selected_chain_id) in [(1, 42161), (42161, 42161), (42161, 1)] {
        let mut chain = chain(None);
        chain.chain_id = website_chain_id;
        let mut request = request_with(
            WalletConnectParsedRequest::WalletSwitchEthereumChain { chain_id: 1 },
            None,
        );
        request.item.chain_id = format!("eip155:{website_chain_id}");
        let mut context = context(&chain, &registry, &rates, &[], &[]);
        context.selected_chain_id = selected_chain_id;
        let intent = build_walletconnect_intent(&request, context);

        assert_eq!(intent.chain_id, 1);
        assert_eq!(
            intent.hero.summary,
            WalletConnectHeroSummary::ChainSwitch {
                from_chain_id: 42161,
                to_chain_id: 1,
            }
        );
    }
}

fn transaction(from: Address, to: Option<Address>, value: U256) -> WalletConnectEvmTransaction {
    WalletConnectEvmTransaction {
        from,
        to,
        value: Some(value),
        data: None,
        access_list: None,
        gas: None,
        gas_price: None,
        max_fee_per_gas: None,
        max_priority_fee_per_gas: None,
        chain_id: Some(1),
        nonce: None,
        transaction_type: None,
        raw: json!({}),
    }
}

fn registry(token: Address) -> EffectiveTokenRegistry {
    let info = EffectiveTokenInfo {
        chain_id: 1,
        token_address: token.to_string(),
        symbol: "USDC".to_owned(),
        decimals: 6,
        icon_path: None,
        price_anchor: None,
        built_in: false,
    };
    EffectiveTokenRegistry {
        tokens: BTreeMap::from([((1, token.to_string().to_ascii_lowercase()), info)]),
    }
}

fn account(
    address: Address,
    label: Option<&str>,
    status: PublicAccountStatus,
) -> PublicAccountMetadata {
    PublicAccountMetadata {
        public_account_uuid: address.to_string(),
        address,
        label: label.map(ToOwned::to_owned),
        source: PublicAccountSource::Imported,
        scope: PublicAccountScope::Global,
        derivation_index: None,
        hardware_descriptor: None,
        status,
        display_order: 0,
    }
}

#[test]
fn token_hit_miss_and_cached_usd_are_explicit() {
    let token = Address::from([0x11; 20]);
    let chain = chain(None);
    let registry = registry(token);
    let rates = TokenAnchorRateCache::new();
    let empty_accounts = Vec::<PublicAccountMetadata>::new();
    let empty_address_book = Vec::<PublicAddressBookEntry>::new();
    let context = context(
        &chain,
        &registry,
        &rates,
        &empty_accounts,
        &empty_address_book,
    );
    let (known, _) = resolve_token_amount(context, Some(token), U256::from(1_500_000_u64), false);
    assert!(
        matches!(&known, WalletConnectAmount::KnownToken { decimals: 6, symbol, usd: None, .. } if symbol == "USDC")
    );
    assert_eq!(
        walletconnect_token_contract_recognition(&known),
        "In token list"
    );
    let (unknown, _) = resolve_token_amount(
        context,
        Some(Address::from([0x22; 20])),
        U256::from(123),
        false,
    );
    assert!(
        matches!(&unknown, WalletConnectAmount::RawToken { raw, unrecognised_contract: true, decimals_unknown: true, .. } if *raw == U256::from(123))
    );
    assert_eq!(
        walletconnect_token_contract_recognition(&unknown),
        "Unrecognised token contract; decimals unknown"
    );
}

#[test]
fn native_and_wrapped_trust_use_the_configured_target() {
    let wrapped = Address::from([0x33; 20]);
    let chain = chain(Some(&wrapped.to_string()));
    assert!(is_trusted_wrapped_native(&chain, Some(wrapped)));
    assert!(!is_trusted_wrapped_native(
        &chain,
        Some(Address::from([0x44; 20]))
    ));
    let registry = EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let rates = TokenAnchorRateCache::new();
    let empty_accounts = Vec::<PublicAccountMetadata>::new();
    let empty_address_book = Vec::<PublicAddressBookEntry>::new();
    let native = native_amount(
        context(
            &chain,
            &registry,
            &rates,
            &empty_accounts,
            &empty_address_book,
        ),
        U256::from(1),
    );
    assert_eq!(native.symbol, "ETH");
    assert!(native.usd.is_none());
}

#[test]
fn unlimited_is_exact_and_large_allowance_stays_finite() {
    let token = Address::from([0x11; 20]);
    let chain = chain(None);
    let registry = registry(token);
    let rates = TokenAnchorRateCache::new();
    let empty_accounts = Vec::<PublicAccountMetadata>::new();
    let empty_address_book = Vec::<PublicAddressBookEntry>::new();
    let context = context(
        &chain,
        &registry,
        &rates,
        &empty_accounts,
        &empty_address_book,
    );
    let (unlimited, _) = resolve_token_amount(context, Some(token), U256::MAX, true);
    let (finite, _) = resolve_token_amount(context, Some(token), U256::MAX - U256::from(1), true);
    assert!(matches!(unlimited, WalletConnectAmount::Unlimited { .. }));
    assert!(matches!(finite, WalletConnectAmount::KnownToken { .. }));
}

#[test]
fn party_identity_follows_ownership_then_address_book_precedence() {
    let selected = Address::from([1; 20]);
    let owned = Address::from([2; 20]);
    let contact = Address::from([3; 20]);
    let unknown = Address::from([4; 20]);
    let accounts = vec![
        account(selected, Some("Selected"), PublicAccountStatus::Active),
        account(
            owned,
            Some("Inactive global"),
            PublicAccountStatus::Inactive,
        ),
    ];
    let address_book = vec![
        PublicAddressBookEntry {
            entry_uuid: "owned-book".to_owned(),
            label: "Book owned".to_owned(),
            address: owned,
            display_order: 0,
        },
        PublicAddressBookEntry {
            entry_uuid: "contact".to_owned(),
            label: "Saved contact".to_owned(),
            address: contact,
            display_order: 1,
        },
    ];
    assert!(matches!(
        party_for(
            WalletConnectPartyRole::Recipient,
            selected,
            selected,
            &accounts,
            &address_book
        )
        .badge,
        WalletConnectPartyBadge::ThisWallet
    ));
    assert!(
        matches!(party_for(WalletConnectPartyRole::Recipient, owned, selected, &accounts, &address_book).badge, WalletConnectPartyBadge::YourAccount { label: Some(label) } if label == "Inactive global")
    );
    assert!(
        matches!(party_for(WalletConnectPartyRole::Recipient, contact, selected, &accounts, &address_book).badge, WalletConnectPartyBadge::AddressBook { label } if label == "Saved contact")
    );
    assert!(matches!(
        party_for(
            WalletConnectPartyRole::Recipient,
            unknown,
            selected,
            &accounts,
            &address_book
        )
        .badge,
        WalletConnectPartyBadge::NotInAddressBook
    ));
    assert!(
        walletconnect_party_badge_label(
            WalletConnectPartyRole::Spender,
            &party_for(
                WalletConnectPartyRole::Spender,
                unknown,
                selected,
                &accounts,
                &address_book,
            )
            .badge,
        )
        .is_none()
    );
    assert!(
        walletconnect_party_badge_label(
            WalletConnectPartyRole::Contract,
            &party_for(
                WalletConnectPartyRole::Contract,
                unknown,
                selected,
                &accounts,
                &address_book,
            )
            .badge,
        )
        .is_none()
    );
    assert!(matches!(
        party_for(
            WalletConnectPartyRole::Spender,
            selected,
            selected,
            &accounts,
            &address_book,
        )
        .badge,
        WalletConnectPartyBadge::ThisWallet
    ));
    assert!(matches!(
        party_for(
            WalletConnectPartyRole::Spender,
            contact,
            selected,
            &accounts,
            &address_book,
        )
        .badge,
        WalletConnectPartyBadge::AddressBook { label } if label == "Saved contact"
    ));
}

#[test]
fn undecoded_attached_native_value_uses_chain_hero_model() {
    let account = Address::from([0x11; 20]);
    let target = Address::from([0x22; 20]);
    let chain = chain(None);
    let registry = registry(target);
    let rates = TokenAnchorRateCache::new();
    let request = request_with(
        WalletConnectParsedRequest::EthSendTransaction {
            transaction: transaction(account, Some(target), U256::from(1)),
        },
        Some(WalletConnectDecodedTransaction {
            target: Some(target),
            native_value: U256::from(1),
            kind: WalletConnectDecodedCallKind::ContractCall {
                selector: Some([0xaa, 0xbb, 0xcc, 0xdd]),
            },
        }),
    );
    let intent = build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));

    assert!(matches!(
        intent.hero.summary,
        WalletConnectHeroSummary::UndecodedCall { .. }
    ));
    assert_eq!(intent.authorization, "Contract call");
    assert!(intent.attached_native.is_some());
    assert!(intent.icon.is_some());
    let summary =
        super::super::requests::walletconnect_request_authorization_summary(&request, &intent);
    let rows = summary.rows_for_test();
    assert!(rows.contains(&("Contract".to_owned(), target.to_checksum(None))));
    assert!(summary.warnings_for_test().iter().any(|warning| {
        warning.contains("Undecoded contract call") && warning.contains("0xaabbccdd")
    }));
}

#[test]
fn personal_previews_are_safe_bounded_or_byte_summaries() {
    assert_eq!(
        personal_message_summary(b"hello").preview.as_deref(),
        Some("hello")
    );
    assert_eq!(
        personal_message_summary(&walletconnect_personal_message_bytes("0x6869"))
            .preview
            .as_deref(),
        Some("hi")
    );
    assert!(personal_message_summary(&[0, 1, 2]).preview.is_none());
    assert!(
        personal_message_summary(b"a\n\n\n\n\n\n\n\n\n\n\n")
            .preview
            .is_none()
    );
    let long = "x".repeat(PERSONAL_SIGN_PREVIEW_MAX_CHARS + 10);
    let preview = personal_message_summary(long.as_bytes())
        .preview
        .expect("preview");
    assert_eq!(preview.chars().count(), PERSONAL_SIGN_PREVIEW_MAX_CHARS);
    assert!(preview.ends_with("..."));
}

#[test]
fn typed_data_domain_fallback_is_neutral() {
    let named = typed_data_summary(&json!({"domain": {"name": "Permit"}, "primaryType": "Permit"}));
    assert_eq!(named.domain_name.as_deref(), Some("Permit"));
    assert_eq!(named.primary_type, "Permit");
    let missing = typed_data_summary(&json!({"domain": {}, "primaryType": "Message"}));
    assert_eq!(missing.domain_name, None);
    assert_eq!(missing.primary_type, "Message");
}

fn permit_typed_data(token: Address, owner: Address, spender: Address, value: &Value) -> Value {
    json!({
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"},
                {"name": "verifyingContract", "type": "address"}
            ],
            "Permit": [
                {"name": "owner", "type": "address"},
                {"name": "spender", "type": "address"},
                {"name": "value", "type": "uint256"},
                {"name": "nonce", "type": "uint256"},
                {"name": "deadline", "type": "uint256"}
            ]
        },
        "primaryType": "Permit",
        "domain": {
            "name": "USD Coin",
            "version": "2",
            "chainId": 1,
            "verifyingContract": token.to_string()
        },
        "message": {
            "owner": owner.to_string(),
            "spender": spender.to_string(),
            "value": value,
            "nonce": 0,
            "deadline": "0x64"
        }
    })
}

#[test]
fn canonical_eip2612_permit_projects_as_a_typed_approval() {
    let account = Address::from([0x11; 20]);
    let token = Address::from([0x22; 20]);
    let spender = Address::from([0x33; 20]);
    let chain = chain(None);
    let registry = registry(token);
    let rates = TokenAnchorRateCache::new();
    let request = request_with(
        WalletConnectParsedRequest::EthSignTypedDataV4 {
            account,
            typed_data: permit_typed_data(token, account, spender, &json!("1000000")),
            domain_chain_id: Some(U256::from(1)),
        },
        None,
    );
    let intent = build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));

    assert_eq!(intent.action, WalletConnectIntentAction::Approve);
    assert!(matches!(
        &intent.amount,
        WalletConnectAmount::KnownToken {
            raw,
            decimals: 6,
            symbol,
            ..
        } if *raw == U256::from(1_000_000_u64) && symbol == "USDC"
    ));
    assert!(matches!(
        &intent.hero.summary,
        WalletConnectHeroSummary::TypedData(summary)
            if summary.domain_name.as_deref() == Some("USD Coin")
                && summary.primary_type == "Permit"
    ));
    assert!(matches!(
        intent.parties.as_slice(),
        [WalletConnectParty {
            role: WalletConnectPartyRole::Spender,
            address,
            ..
        }] if *address == spender
    ));
    assert!(intent.authorization.contains("1 USDC"));
    assert!(!intent.authorization.contains("Spender can withdraw up to"));
    let summary =
        super::super::requests::walletconnect_request_authorization_summary(&request, &intent);
    assert!(
        summary
            .rows_for_test()
            .contains(&("Typed data".to_owned(), "USD Coin / Permit".to_owned()))
    );
}

#[test]
fn keys_the_signer_ignores_do_not_hide_an_unlimited_permit() {
    let account = Address::from([0x11; 20]);
    let token = Address::from([0x22; 20]);
    let spender = Address::from([0x33; 20]);
    let chain = chain(None);
    let registry = registry(token);
    let rates = TokenAnchorRateCache::new();
    let mut typed_data = permit_typed_data(token, account, spender, &json!(U256::MAX.to_string()));
    typed_data["message"]["memo"] = json!("x");
    typed_data["types"]["Permit"][0]["note"] = json!("x");
    let request = request_with(
        WalletConnectParsedRequest::EthSignTypedDataV4 {
            account,
            typed_data,
            domain_chain_id: Some(U256::from(1)),
        },
        None,
    );
    let intent = build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));

    assert_eq!(intent.action, WalletConnectIntentAction::Approve);
    assert_eq!(
        intent.risks,
        [WalletConnectRisk::UnlimitedAllowance { spender }]
    );
}

#[test]
fn dai_style_permit_with_allowed_true_is_an_unlimited_approval() {
    let account = Address::from([0x11; 20]);
    let token = Address::from([0x22; 20]);
    let spender = Address::from([0x33; 20]);
    let chain = chain(None);
    let registry = registry(token);
    let rates = TokenAnchorRateCache::new();
    let mut typed_data = permit_typed_data(token, account, spender, &json!("0"));
    typed_data["types"]["Permit"] = json!([
        {"name": "holder", "type": "address"},
        {"name": "spender", "type": "address"},
        {"name": "nonce", "type": "uint256"},
        {"name": "expiry", "type": "uint256"},
        {"name": "allowed", "type": "bool"}
    ]);
    typed_data["message"] = json!({
        "holder": account.to_string(),
        "spender": spender.to_string(),
        "nonce": 0,
        "expiry": 0,
        "allowed": true
    });
    let request = request_with(
        WalletConnectParsedRequest::EthSignTypedDataV4 {
            account,
            typed_data,
            domain_chain_id: Some(U256::from(1)),
        },
        None,
    );
    let intent = build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));

    assert_eq!(intent.action, WalletConnectIntentAction::Approve);
    assert_eq!(
        intent.risks,
        [WalletConnectRisk::UnlimitedAllowance { spender }]
    );
}

#[test]
fn noncanonical_or_foreign_permit_keeps_generic_typed_data_behavior() {
    let account = Address::from([0x11; 20]);
    let token = Address::from([0x22; 20]);
    let spender = Address::from([0x33; 20]);
    let foreign_owner = Address::from([0x44; 20]);
    let chain = chain(None);
    let registry = registry(token);
    let rates = TokenAnchorRateCache::new();
    let request = request_with(
        WalletConnectParsedRequest::EthSignTypedDataV4 {
            account,
            typed_data: permit_typed_data(
                token,
                foreign_owner,
                spender,
                &json!(U256::MAX.to_string()),
            ),
            domain_chain_id: Some(U256::from(1)),
        },
        None,
    );
    let intent = build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));

    assert_eq!(intent.action, WalletConnectIntentAction::TypedDataSign);
    assert!(matches!(
        intent.hero.summary,
        WalletConnectHeroSummary::TypedData(_)
    ));
    assert!(matches!(
        intent.parties.as_slice(),
        [WalletConnectParty {
            role: WalletConnectPartyRole::Signer,
            address,
            ..
        }] if *address == account
    ));
    assert_eq!(intent.risks, [] as [WalletConnectRisk; 0]);
}

#[test]
fn token_risks_cover_maximum_foreign_and_attached_value_rules() {
    let selected = Address::from([1; 20]);
    let spender = Address::from([2; 20]);
    let foreign = Address::from([3; 20]);
    let native = WalletConnectNativeAmount {
        raw: U256::from(1),
        symbol: "ETH".to_owned(),
        display: "1 ETH".to_owned(),
        usd: None,
    };
    let allowance = token_operation_risks(
        &WalletConnectDecodedCallKind::Erc20Approve {
            spender,
            amount: U256::MAX,
        },
        Some(&native),
        selected,
    );
    assert!(matches!(
        allowance.as_slice(),
        [
            WalletConnectRisk::UnlimitedAllowance { .. },
            WalletConnectRisk::AttachedNativeValue(_)
        ]
    ));
    let equal_source = token_operation_risks(
        &WalletConnectDecodedCallKind::Erc20TransferFrom {
            from: selected,
            to: Address::ZERO,
            amount: U256::from(1),
        },
        None,
        selected,
    );
    assert_eq!(equal_source, [] as [WalletConnectRisk; 0]);
    let foreign_source = token_operation_risks(
        &WalletConnectDecodedCallKind::Erc20TransferFrom {
            from: foreign,
            to: Address::ZERO,
            amount: U256::from(1),
        },
        None,
        selected,
    );
    assert!(
        matches!(foreign_source.as_slice(), [WalletConnectRisk::ForeignTransferSource { source }] if *source == foreign)
    );
}

#[test]
fn fee_significance_omits_missing_or_zero_values() {
    for (expected, moving) in [
        (None, Some(U256::from(100))),
        (Some(U256::from(25)), None),
        (Some(U256::from(25)), Some(U256::ZERO)),
    ] {
        assert_eq!(
            classify_walletconnect_fee_significance(expected, moving),
            WalletConnectFeeSignificance::Normal
        );
    }
}

#[test]
fn moving_value_requires_finite_known_amount_and_cached_price() {
    let token = Address::from([0x55; 20]);
    let rates = TokenAnchorRateCache::new();
    rates.store_native_usd_rate(1, U256::from(1_000_000_u64), 18);
    rates.store_rate(1, token, U256::from(1_000_000_000_000_000_000_u64));
    let known_token = WalletConnectAmount::KnownToken {
        token,
        raw: U256::from(1_000_000_000_000_000_000_u128),
        decimals: 18,
        symbol: "TOK".to_owned(),
        display: "1 TOK".to_owned(),
        usd: None,
    };
    assert!(
        walletconnect_moving_usd_micro_value(
            1,
            WalletConnectIntentAction::TokenTransfer,
            &known_token,
            None,
            &rates,
        )
        .is_some()
    );
    let native = WalletConnectAmount::Native {
        raw: U256::from(1_000_000_000_000_000_000_u128),
        symbol: "ETH".to_owned(),
        display: "1 ETH".to_owned(),
        usd: None,
    };
    let attached_native = WalletConnectNativeAmount {
        raw: U256::from(1_000_000_000_000_000_000_u128),
        symbol: "ETH".to_owned(),
        display: "1 ETH".to_owned(),
        usd: None,
    };
    assert_eq!(
        walletconnect_moving_usd_micro_value(
            1,
            WalletConnectIntentAction::NativeTransfer,
            &native,
            Some(&attached_native),
            &rates,
        ),
        Some(U256::from(1_000_000_u64))
    );
    assert_eq!(
        walletconnect_moving_usd_micro_value(
            1,
            WalletConnectIntentAction::Wrap,
            &native,
            Some(&attached_native),
            &rates,
        ),
        Some(U256::from(1_000_000_u64))
    );
    assert_eq!(
        walletconnect_moving_usd_micro_value(
            1,
            WalletConnectIntentAction::TokenTransfer,
            &known_token,
            Some(&attached_native),
            &rates,
        ),
        Some(U256::from(2_000_000_u64))
    );
    let attached_without_cached_value = WalletConnectNativeAmount {
        raw: U256::MAX,
        symbol: "ETH".to_owned(),
        display: "unavailable".to_owned(),
        usd: None,
    };
    assert!(
        walletconnect_moving_usd_micro_value(
            1,
            WalletConnectIntentAction::TokenTransfer,
            &known_token,
            Some(&attached_without_cached_value),
            &rates,
        )
        .is_none()
    );
    assert!(
        walletconnect_moving_usd_micro_value(
            1,
            WalletConnectIntentAction::NativeTransfer,
            &native,
            None,
            &rates,
        )
        .is_some()
    );
    let raw = WalletConnectAmount::RawToken {
        token,
        raw: U256::from(1),
        display: "1 raw token unit".to_owned(),
        unrecognised_contract: true,
        decimals_unknown: true,
    };
    assert!(
        walletconnect_moving_usd_micro_value(
            1,
            WalletConnectIntentAction::TokenTransfer,
            &raw,
            None,
            &rates,
        )
        .is_none()
    );
    assert!(
        walletconnect_moving_usd_micro_value(
            1,
            WalletConnectIntentAction::Approve,
            &known_token,
            None,
            &rates,
        )
        .is_none()
    );
}
