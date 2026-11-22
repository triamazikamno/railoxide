use super::*;

#[test]
fn common_governance_decoded_previews_show_parties_tokens_and_rates() {
    let proxy = address!("0x2222222222222222222222222222222222222222");
    let implementation = address!("0x3333333333333333333333333333333333333333");
    let account = address!("0x4444444444444444444444444444444444444444");
    let token = railgun_ui::governance_contracts(1)
        .expect("Ethereum governance")
        .governance_token;
    let usdt = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([
            (
                (1, token.to_string().to_ascii_lowercase()),
                wallet_ops::settings::EffectiveTokenInfo {
                    chain_id: 1,
                    token_address: token.to_string(),
                    symbol: "RAIL".to_owned(),
                    decimals: 18,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            ),
            (
                (1, usdt.to_string().to_ascii_lowercase()),
                wallet_ops::settings::EffectiveTokenInfo {
                    chain_id: 1,
                    token_address: usdt.to_string(),
                    symbol: "USDT".to_owned(),
                    decimals: 6,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            ),
            (
                (1, usdc.to_string().to_ascii_lowercase()),
                wallet_ops::settings::EffectiveTokenInfo {
                    chain_id: 1,
                    token_address: usdc.to_string(),
                    symbol: "USDC".to_owned(),
                    decimals: 6,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            ),
        ]),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    let upgrade = proposal_decoded_preview(
        &DecodedProposalAction::ProxyUpgrade {
            proxy,
            implementation,
        },
        address!("0x1111111111111111111111111111111111111111"),
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(upgrade.verb, "Upgrade proxy");
    assert!(matches!(
        upgrade.details.as_slice(),
        [
            ProposalDecodedDetail::Party { role: proxy_role, address: proxy_address, .. },
            ProposalDecodedDetail::Party {
                role: implementation_role,
                address: implementation_address,
                ..
            },
        ] if proxy_role == "PROXY"
            && *proxy_address == proxy
            && implementation_role == "IMPLEMENTATION"
            && *implementation_address == implementation
    ));

    let ownership = proposal_decoded_preview(
        &DecodedProposalAction::TransferOwnership { new_owner: account },
        address!("0x1111111111111111111111111111111111111111"),
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(ownership.verb, "Transfer ownership");
    assert!(ownership.hero.is_none());
    assert!(matches!(
        &ownership.details[0],
        ProposalDecodedDetail::Party { role, address, .. }
            if role == "NEW OWNER" && *address == account
    ));

    let pause = proposal_decoded_preview(
        &DecodedProposalAction::ProxyPause { proxy },
        address!("0x1111111111111111111111111111111111111111"),
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(pause.verb, "Pause proxy");
    assert!(matches!(
        &pause.details[0],
        ProposalDecodedDetail::Party { role, address, .. }
            if role == "PROXY" && *address == proxy
    ));

    let proxy_ownership = proposal_decoded_preview(
        &DecodedProposalAction::ProxyTransferOwnership {
            proxy,
            new_owner: account,
        },
        address!("0x1111111111111111111111111111111111111111"),
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(proxy_ownership.verb, "Transfer proxy ownership");
    assert!(matches!(
        proxy_ownership.details.as_slice(),
        [
            ProposalDecodedDetail::Party { role: proxy_role, address: proxy_address, .. },
            ProposalDecodedDetail::Party { role: owner_role, address: owner_address, .. },
        ] if proxy_role == "PROXY"
            && *proxy_address == proxy
            && owner_role == "NEW OWNER"
            && *owner_address == account
    ));

    let fees = proposal_decoded_preview(
        &DecodedProposalAction::ChangeFee {
            shield_fee: U256::from(10),
            unshield_fee: U256::from(10),
            nft_fee: U256::ZERO,
        },
        address!("0x5555555555555555555555555555555555555555"),
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(fees.verb, "Change protocol fees");
    assert!(fees.hero.is_none());
    assert!(matches!(
        fees.details.as_slice(),
        [
            ProposalDecodedDetail::Value { role: shield_role, value: shield_value, copy_value: Some(shield_copy), .. },
            ProposalDecodedDetail::Value { role: unshield_role, value: unshield_value, copy_value: Some(unshield_copy), .. },
            ProposalDecodedDetail::Value { role: nft_role, value: nft_value, copy_value: Some(nft_copy), .. },
        ] if shield_role == "SHIELD FEE"
            && shield_value == "0.10% (10 bp)"
            && shield_copy == "10"
            && unshield_role == "UNSHIELD FEE"
            && unshield_value == "0.10% (10 bp)"
            && unshield_copy == "10"
            && nft_role == "NFT FEE"
            && nft_value == "0 ETH (0 wei)"
            && nft_copy == "0"
    ));

    let mint = proposal_decoded_preview(
        &DecodedProposalAction::GovernanceMint {
            account,
            amount: U256::from(10_000_000_000_000_000_000_000_u128),
        },
        token,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(mint.verb, "Mint governance tokens");
    assert_eq!(
        mint.hero.as_ref().map(|hero| hero.rounded.as_str()),
        Some("10,000 RAIL")
    );
    assert!(matches!(
        &mint.details[0],
        ProposalDecodedDetail::Party { role, address, .. }
            if role == "TO" && *address == account
    ));

    let interval = proposal_decoded_preview(
        &DecodedProposalAction::SetIntervalBP {
            new_interval_bp: U256::from(420),
        },
        Address::ZERO,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(interval.verb, "Set reward interval rate");
    assert!(matches!(
        &interval.details[0],
        ProposalDecodedDetail::Value {
            role,
            value,
            copy_value: Some(copy_value),
            monospace,
        } if role == "RATE"
            && value == "4.20% (420 bp)"
            && copy_value == "420"
            && !monospace
    ));

    let add_tokens = proposal_decoded_preview(
        &DecodedProposalAction::AddTokens {
            tokens: vec![usdt, usdc],
        },
        railgun_ui::governance_contracts(1)
            .expect("Ethereum governance")
            .governor_rewards,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(add_tokens.verb, "Add reward tokens");
    assert!(add_tokens.hero.is_none());
    assert!(matches!(
        add_tokens.details.as_slice(),
        [
            ProposalDecodedDetail::Party {
                role: first_role,
                address: first_address,
                badge: Some(first_badge),
                ..
            },
            ProposalDecodedDetail::Party {
                role: second_role,
                address: second_address,
                badge: Some(second_badge),
                ..
            },
        ] if first_role == "TOKEN"
            && *first_address == usdt
            && first_badge == "USDT"
            && second_role == "TOKEN"
            && *second_address == usdc
            && second_badge == "USDC"
    ));

    let caller = address!("0x64DA0892E8E24fECa6Eb5E3D8cbf2D9b6Fbe7598");
    let contract_address = address!("0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9");
    let known_selector = FixedBytes::from([0x2e, 0xc0, 0xf3, 0x59]);
    let grant = proposal_decoded_preview(
        &DecodedProposalAction::DelegatorSetPermission {
            caller,
            contract_address,
            selector: known_selector,
            permission: true,
        },
        railgun_ui::governance_contracts(1)
            .expect("Ethereum governance")
            .delegator,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(grant.verb, "Grant call permission");
    assert!(grant.hero.is_none());
    assert!(matches!(
        grant.details.as_slice(),
        [
            ProposalDecodedDetail::Party { role: caller_role, address: caller_value, .. },
            ProposalDecodedDetail::Party {
                role: contract_role,
                address: contract_value,
                ..
            },
            ProposalDecodedDetail::Value {
                role: function_role,
                value,
                copy_value: Some(copy_value),
                monospace,
            },
        ] if caller_role == "CALLER"
            && *caller_value == caller
            && contract_role == "CONTRACT"
            && *contract_value == contract_address
            && function_role == "FUNCTION"
            && value == "setVerificationKey · 0x2ec0f359"
            && copy_value == "0x2ec0f359"
            && *monospace
    ));

    let unknown_selector = FixedBytes::from([0xaa, 0xbb, 0xcc, 0xdd]);
    let revoke = proposal_decoded_preview(
        &DecodedProposalAction::DelegatorSetPermission {
            caller,
            contract_address,
            selector: unknown_selector,
            permission: false,
        },
        Address::ZERO,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(revoke.verb, "Revoke call permission");
    assert!(matches!(
        &revoke.details[2],
        ProposalDecodedDetail::Value {
            role,
            value,
            copy_value: Some(copy_value),
            ..
        } if role == "FUNCTION" && value == "0xaabbccdd" && copy_value == "0xaabbccdd"
    ));
}

#[test]
fn delegator_wildcard_preview_details_preserve_specific_counterpart() {
    let caller = address!("0x64DA0892E8E24fECa6Eb5E3D8cbf2D9b6Fbe7598");
    let contract = address!("0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9");
    let known_selector = FixedBytes::from([0x2e, 0xc0, 0xf3, 0x59]);
    let zero_selector = FixedBytes::from([0; 4]);
    let zero_copy = Address::ZERO.to_checksum(None);
    let contract_copy = contract.to_checksum(None);
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    let cases = [
        (
            "zero contract only",
            Address::ZERO,
            known_selector,
            true,
            "Any contract · 0x0000…0000",
            zero_copy.as_str(),
            "setVerificationKey · 0x2ec0f359",
            "0x2ec0f359",
        ),
        (
            "zero selector only",
            contract,
            zero_selector,
            false,
            "",
            contract_copy.as_str(),
            "Any function · 0x00000000",
            "0x00000000",
        ),
        (
            "both zero",
            Address::ZERO,
            zero_selector,
            true,
            "Any contract · 0x0000…0000",
            zero_copy.as_str(),
            "Any function · 0x00000000",
            "0x00000000",
        ),
    ];

    for (
        label,
        contract_address,
        selector,
        wildcard_contract,
        expected_contract_value,
        expected_contract_copy,
        expected_function_value,
        expected_function_copy,
    ) in cases
    {
        let preview = proposal_decoded_preview(
            &DecodedProposalAction::DelegatorSetPermission {
                caller,
                contract_address,
                selector,
                permission: true,
            },
            Address::ZERO,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert!(
            matches!(
                &preview.details[0],
                ProposalDecodedDetail::Party {
                    role,
                    address,
                    copy_value,
                    ..
                } if role == "CALLER"
                    && *address == caller
                    && copy_value == caller.to_checksum(None).as_str()
            ),
            "{label}: caller detail"
        );

        match (&preview.details[1], wildcard_contract) {
            (
                ProposalDecodedDetail::Value {
                    role,
                    value,
                    copy_value: Some(copy_value),
                    monospace,
                },
                true,
            ) if role == "CONTRACT"
                && value == expected_contract_value
                && copy_value == expected_contract_copy
                && !monospace => {}
            (
                ProposalDecodedDetail::Party {
                    role,
                    address,
                    copy_value,
                    ..
                },
                false,
            ) if role == "CONTRACT"
                && *address == contract
                && copy_value == expected_contract_copy => {}
            _ => panic!("{label}: unexpected contract detail"),
        }

        assert!(
            matches!(
                &preview.details[2],
                ProposalDecodedDetail::Value {
                    role,
                    value,
                    copy_value: Some(copy_value),
                    monospace,
                } if role == "FUNCTION"
                    && value == expected_function_value
                    && copy_value == expected_function_copy
                    && *monospace
            ),
            "{label}: function detail"
        );
    }
}

#[test]
fn proposal_decoded_preview_uses_typed_token_and_party_presentation() {
    let target = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([(
            (1, target.to_string().to_ascii_lowercase()),
            wallet_ops::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: target.to_string(),
                symbol: "TEST".to_owned(),
                decimals: 6,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    let transfer = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Transfer {
            recipient,
            amount: U256::from(15_109_211_424_u64),
        },
        target,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        Some(&[]),
    );
    let hero = transfer.hero.as_ref().expect("known token hero");
    assert_eq!(hero.rounded, "15,109 TEST");
    assert!(hero.icon.is_none(), "custom icon-less symbols have no slot");
    assert!(hero.amount_copy_value.is_none());
    assert!(hero.context_copy_value.is_none());
    assert_eq!(transfer.details.len(), 1);
    assert!(matches!(
        &transfer.details[0],
        ProposalDecodedDetail::Party { role, address, badge, .. }
            if role == "TO" && *address == recipient && badge.is_none()
    ));

    let unknown_token = address!("0x9999999999999999999999999999999999999999");
    let unknown = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Transfer {
            recipient,
            amount: U256::from(42),
        },
        unknown_token,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    let unknown_hero = unknown.hero.as_ref().expect("unknown token hero");
    assert!(unknown_hero.icon.is_none());
    assert_eq!(unknown_hero.rounded, "42 raw token units");
    assert_eq!(unknown_hero.amount_copy_value.as_deref(), Some("42"));
    let unknown_token_short = railgun_ui::short_address(&unknown_token);
    assert!(
        unknown_hero
            .context
            .as_deref()
            .is_some_and(|context| context.contains(unknown_token_short.as_str()))
    );
    let unknown_token_checksum = unknown_token.to_checksum(None);
    assert_eq!(
        unknown_hero.context_copy_value.as_deref(),
        Some(unknown_token_checksum.as_str())
    );
}
