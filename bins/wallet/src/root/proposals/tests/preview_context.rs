use super::*;

#[test]
fn proposal_decoded_preview_adds_only_nonredundant_cached_usd_context() {
    let target = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([(
            (1, target.to_string().to_ascii_lowercase()),
            wallet_ops::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: target.to_string(),
                symbol: "WBTC".to_owned(),
                decimals: 8,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    anchor_rates.store_rate(1, target, U256::from(100_000_000_u64));
    anchor_rates.store_native_usd_rate(1, U256::from(13_194_600_000_u64), 18);
    let priced = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Transfer {
            recipient,
            amount: U256::from(100_000_000_u64),
        },
        target,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(
        priced
            .hero
            .as_ref()
            .and_then(|hero| hero.context.as_deref()),
        Some("≈ $13,194.60")
    );

    let stable_target = address!("0x3333333333333333333333333333333333333333");
    let stable_registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([(
            (1, stable_target.to_string().to_ascii_lowercase()),
            wallet_ops::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: stable_target.to_string(),
                symbol: "USDC".to_owned(),
                decimals: 6,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]),
    };
    let stable_rates = TokenAnchorRateCache::new();
    stable_rates.store_rate(1, stable_target, U256::from(1_000_000_000_u64));
    stable_rates.store_native_usd_rate(1, U256::from(1_000_000_000_u64), 18);
    let stable = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Transfer {
            recipient,
            amount: U256::from(10_000_000_u64),
        },
        stable_target,
        U256::ZERO,
        1,
        &stable_rates,
        &stable_registry,
        None,
        &[],
        None,
    );
    assert!(
        stable
            .hero
            .as_ref()
            .is_some_and(|hero| hero.context.is_none())
    );

    let missing_rates = TokenAnchorRateCache::new();
    let missing = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Transfer {
            recipient,
            amount: U256::from(100_000_000_u64),
        },
        target,
        U256::ZERO,
        1,
        &missing_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert!(
        missing
            .hero
            .as_ref()
            .is_some_and(|hero| hero.context.is_none())
    );
}

#[test]
fn proposal_decoded_preview_preserves_direction_treasury_badges_and_approval_warning() {
    let target = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let from = address!("0x3333333333333333333333333333333333333333");
    let spender = address!("0x52908400098527886e0f7030069857d2e4169ee7");
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([(
            (1, target.to_string().to_ascii_lowercase()),
            wallet_ops::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: target.to_string(),
                symbol: "TEST".to_owned(),
                decimals: 2,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    let transfer_from = proposal_decoded_preview(
        &DecodedProposalAction::Erc20TransferFrom {
            from,
            to: recipient,
            amount: U256::from(1),
        },
        target,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert!(matches!(
        transfer_from.details.as_slice(),
        [
            ProposalDecodedDetail::Party { role: from_role, address: from_address, .. },
            ProposalDecodedDetail::Connector,
            ProposalDecodedDetail::Party { role: to_role, address: to_address, .. },
        ] if from_role == "FROM" && *from_address == from && to_role == "TO" && *to_address == recipient
    ));

    let treasury = railgun_ui::governance_treasury(1).expect("ethereum treasury metadata");
    let treasury_transfer = proposal_decoded_preview(
        &DecodedProposalAction::TreasuryTransferErc20 {
            token: target,
            to: recipient,
            amount: U256::from(1),
        },
        treasury,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert!(matches!(
        &treasury_transfer.details[0],
        ProposalDecodedDetail::Party { role, address, badge: Some(badge), .. }
            if role == "FROM" && *address == treasury && badge == "Treasury"
    ));

    let bounded = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Approve {
            spender,
            amount: U256::from(1),
        },
        target,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert!(bounded.unlimited_warning.is_none());
    assert!(!bounded.hero.as_ref().expect("approval hero").danger);

    let unlimited = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Approve {
            spender,
            amount: U256::MAX,
        },
        target,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    let unlimited_hero = unlimited.hero.as_ref().expect("approval hero");
    assert_eq!(unlimited_hero.rounded, "Unlimited TEST");
    assert!(unlimited_hero.danger);
    let expected_warning = format!(
        "Unlimited allowance: {} can keep spending this token until the allowance is revoked.",
        spender.to_checksum(None)
    );
    assert_eq!(
        unlimited.unlimited_warning.as_deref(),
        Some(expected_warning.as_str())
    );
    assert!(matches!(
        &unlimited.details[0],
        ProposalDecodedDetail::Party { role, address, .. }
            if role == "SPENDER" && *address == spender
    ));
}

#[test]
fn proposal_decoded_preview_covers_wrapping_and_role_shells() {
    let target = address!("0x1111111111111111111111111111111111111111");
    let account = address!("0x2222222222222222222222222222222222222222");
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    let wrapped = proposal_decoded_preview(
        &DecodedProposalAction::WrappedDeposit,
        target,
        U256::from(1_000_000_000_000_000_000_u128),
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    let wrapped_hero = wrapped.hero.as_ref().expect("wrap hero");
    assert_eq!(wrapped.verb, "Wrap");
    assert_eq!(wrapped_hero.rounded, "1 ETH");
    assert_eq!(wrapped_hero.context.as_deref(), Some("wrapped into WETH"));

    let role = B256::repeat_byte(7);
    let role_preview = proposal_decoded_preview(
        &DecodedProposalAction::TreasuryGrantRole { role, account },
        target,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(role_preview.details.len(), 2);
    assert!(matches!(
        &role_preview.details[0],
        ProposalDecodedDetail::Value { role: label, value, .. }
            if label == "ROLE" && value == &proposal_role_label(role)
    ));
    assert!(matches!(
        &role_preview.details[1],
        ProposalDecodedDetail::Party { role, address, .. }
            if role == "ACCOUNT" && *address == account
    ));
}

#[test]
fn proposal_decoded_preview_retains_exact_address_copy_fidelity() {
    let first = address!("0x123400000000000000000000000000000000abcd");
    let second = address!("0x123411111111111111111111111111111111abcd");
    assert_eq!(
        railgun_ui::short_address(&first),
        railgun_ui::short_address(&second)
    );
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    let preview = proposal_decoded_preview(
        &DecodedProposalAction::TreasuryInitialize { owner: first },
        Address::ZERO,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert!(matches!(
        &preview.details[0],
        ProposalDecodedDetail::Party { address, copy_value, .. }
            if *address == first && copy_value == &first.to_checksum(None)
    ));
    let other = proposal_decoded_preview(
        &DecodedProposalAction::TreasuryInitialize { owner: second },
        Address::ZERO,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert!(matches!(
        &other.details[0],
        ProposalDecodedDetail::Party { address, copy_value, .. }
            if *address == second && copy_value == &second.to_checksum(None)
    ));
}
