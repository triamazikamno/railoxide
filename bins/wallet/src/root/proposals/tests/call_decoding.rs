use super::*;

#[test]
fn calldata_display_preserves_exact_copy_source_and_compact_preview() {
    let calldata = alloy::primitives::Bytes::from(vec![0xab; 40]);
    let exact = action_calldata_hex(&calldata);
    assert_eq!(exact, format!("0x{}", "ab".repeat(40)));
    assert_eq!(
        compact_calldata_display(&exact),
        "0xababababababababab…abababababababab"
    );
}

#[test]
fn proposal_decodes_generic_erc20_calls_to_semantic_fields() {
    let target = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let from = address!("0x3333333333333333333333333333333333333333");
    let amount = U256::from(42);
    assert_eq!(
        decode_proposal_action(
            1,
            target,
            &ProposalErc20::transferCall { recipient, amount }.abi_encode(),
            None,
            None,
        ),
        Some(DecodedProposalAction::Erc20Transfer { recipient, amount })
    );
    assert_eq!(
        decode_proposal_action(
            1,
            target,
            &ProposalErc20::transferFromCall {
                from,
                to: recipient,
                amount,
            }
            .abi_encode(),
            None,
            None,
        ),
        Some(DecodedProposalAction::Erc20TransferFrom {
            from,
            to: recipient,
            amount,
        })
    );
    let spender = address!("0x4444444444444444444444444444444444444444");
    assert_eq!(
        decode_proposal_action(
            1,
            target,
            &ProposalErc20::approveCall { spender, amount }.abi_encode(),
            None,
            None,
        ),
        Some(DecodedProposalAction::Erc20Approve { spender, amount })
    );
}

#[test]
fn proposal_decodes_common_governance_calls_with_target_guards() {
    let arbitrary_target = address!("0x1111111111111111111111111111111111111111");
    let proxy = address!("0x2222222222222222222222222222222222222222");
    let implementation = address!("0x3333333333333333333333333333333333333333");
    let account = address!("0x4444444444444444444444444444444444444444");
    let upgrade = ProposalProxyAdmin::upgradeCall {
        proxy,
        implementation,
    }
    .abi_encode();
    let upgrade_decoded = DecodedProposalAction::ProxyUpgrade {
        proxy,
        implementation,
    };
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &upgrade, None, None),
        Some(upgrade_decoded.clone())
    );
    let empty_registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    assert_eq!(
        proposal_action_target_label(
            1,
            arbitrary_target,
            &empty_registry,
            None,
            Some(&upgrade_decoded),
        ),
        Some("Proxy admin".to_owned())
    );

    let pause = ProposalProxyAdmin::pauseCall { proxy }.abi_encode();
    assert_eq!(&pause[..4], &[0x76, 0xa6, 0x7a, 0x51]);
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &pause, None, None),
        Some(DecodedProposalAction::ProxyPause { proxy })
    );
    let proxy_owner = ProposalProxyAdmin::transferProxyOwnershipCall {
        proxy,
        newOwner: account,
    }
    .abi_encode();
    assert_eq!(&proxy_owner[..4], &[0x00, 0x36, 0x1d, 0x55]);
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &proxy_owner, None, None),
        Some(DecodedProposalAction::ProxyTransferOwnership {
            proxy,
            new_owner: account,
        })
    );

    let railgun = address!("0x5555555555555555555555555555555555555555");
    let change_fee = ProposalRailgun::changeFeeCall {
        shieldFee: alloy::primitives::Uint::<120, 2>::from_limbs([10, 0]),
        unshieldFee: alloy::primitives::Uint::<120, 2>::from_limbs([10, 0]),
        nftFee: U256::ZERO,
    }
    .abi_encode();
    assert_eq!(&change_fee[..4], &[0xcc, 0x1f, 0x73, 0xfd]);
    assert_eq!(
        decode_proposal_action(1, railgun, &change_fee, None, Some(railgun)),
        Some(DecodedProposalAction::ChangeFee {
            shield_fee: U256::from(10),
            unshield_fee: U256::from(10),
            nft_fee: U256::ZERO,
        })
    );
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &change_fee, None, Some(railgun)),
        None
    );
    let mut trailing_change_fee = change_fee;
    trailing_change_fee.push(0);
    assert_eq!(
        decode_proposal_action(1, railgun, &trailing_change_fee, None, Some(railgun)),
        None
    );

    let contracts = railgun_ui::governance_contracts(1).expect("Ethereum governance");
    let mint = ProposalGovernanceToken::governanceMintCall {
        account,
        amount: U256::from(42),
    }
    .abi_encode();
    assert_eq!(
        decode_proposal_action(1, contracts.governance_token, &mint, None, None),
        Some(DecodedProposalAction::GovernanceMint {
            account,
            amount: U256::from(42),
        })
    );
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &mint, None, None),
        None
    );

    let interval = ProposalGovernorRewards::setIntervalBPCall {
        newIntervalBP: U256::from(420),
    }
    .abi_encode();
    assert_eq!(
        decode_proposal_action(1, contracts.governor_rewards, &interval, None, None),
        Some(DecodedProposalAction::SetIntervalBP {
            new_interval_bp: U256::from(420),
        })
    );
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &interval, None, None),
        None
    );

    let usdt = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let add_tokens = ProposalGovernorRewards::addTokensCall {
        tokens: vec![usdt, usdc],
    }
    .abi_encode();
    assert_eq!(&add_tokens[..4], [0x4a, 0xe0, 0x5c, 0x7d]);
    assert_eq!(
        decode_proposal_action(1, contracts.governor_rewards, &add_tokens, None, None),
        Some(DecodedProposalAction::AddTokens {
            tokens: vec![usdt, usdc],
        })
    );
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &add_tokens, None, None),
        None
    );
    let mut malformed_offset = add_tokens.clone();
    malformed_offset[4..36].fill(0);
    assert_eq!(
        decode_proposal_action(1, contracts.governor_rewards, &malformed_offset, None, None),
        None
    );
    let mut trailing_tokens = add_tokens;
    trailing_tokens.push(0);
    assert_eq!(
        decode_proposal_action(1, contracts.governor_rewards, &trailing_tokens, None, None),
        None
    );

    let caller = address!("0x64DA0892E8E24fECa6Eb5E3D8cbf2D9b6Fbe7598");
    let contract_address = address!("0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9");
    let selector = FixedBytes::from([0x2e, 0xc0, 0xf3, 0x59]);
    let permission = ProposalDelegator::setPermissionCall {
        caller,
        contractAddress: contract_address,
        selector,
        permission: true,
    }
    .abi_encode();
    assert_eq!(&permission[..4], [0xe6, 0x46, 0x24, 0xfa]);
    let permission_decoded = DecodedProposalAction::DelegatorSetPermission {
        caller,
        contract_address,
        selector,
        permission: true,
    };
    assert_eq!(
        decode_proposal_action(1, contracts.delegator, &permission, None, None),
        Some(permission_decoded)
    );
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &permission, None, None),
        None
    );

    let mut malformed_bool = permission.clone();
    malformed_bool[131] = 2;
    assert_eq!(
        decode_proposal_action(1, contracts.delegator, &malformed_bool, None, None),
        None
    );
    let mut malformed_address = permission.clone();
    malformed_address[4] = 1;
    assert_eq!(
        decode_proposal_action(1, contracts.delegator, &malformed_address, None, None),
        None
    );
    let mut malformed_padding = permission.clone();
    malformed_padding[72] = 1;
    assert_eq!(
        decode_proposal_action(1, contracts.delegator, &malformed_padding, None, None),
        None
    );
    let mut trailing_permission = permission.clone();
    trailing_permission.push(0);
    assert_eq!(
        decode_proposal_action(1, contracts.delegator, &trailing_permission, None, None),
        None
    );
    assert_eq!(
        decode_proposal_action(
            1,
            contracts.delegator,
            &permission[..permission.len() - 1],
            None,
            None
        ),
        None
    );

    let mut trailing = upgrade;
    trailing.push(0);
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &trailing, None, None),
        None
    );
}

#[test]
fn proposal_decodes_all_treasury_writes_only_at_known_target() {
    let treasury = railgun_ui::governance_treasury(1).expect("ethereum treasury metadata");
    let other = address!("0x1111111111111111111111111111111111111111");
    let token = address!("0x2222222222222222222222222222222222222222");
    let account = address!("0x3333333333333333333333333333333333333333");
    let role = B256::repeat_byte(7);
    let calls = [
        (
            ProposalTreasury::transferERC20Call {
                token,
                to: account,
                amount: U256::from(1),
            }
            .abi_encode(),
            DecodedProposalAction::TreasuryTransferErc20 {
                token,
                to: account,
                amount: U256::from(1),
            },
        ),
        (
            ProposalTreasury::transferETHCall {
                to: account,
                amount: U256::from(2),
            }
            .abi_encode(),
            DecodedProposalAction::TreasuryTransferEth {
                to: account,
                amount: U256::from(2),
            },
        ),
        (
            ProposalTreasury::initializeTreasuryCall { owner: account }.abi_encode(),
            DecodedProposalAction::TreasuryInitialize { owner: account },
        ),
        (
            ProposalTreasury::grantRoleCall { role, account }.abi_encode(),
            DecodedProposalAction::TreasuryGrantRole { role, account },
        ),
        (
            ProposalTreasury::revokeRoleCall { role, account }.abi_encode(),
            DecodedProposalAction::TreasuryRevokeRole { role, account },
        ),
        (
            ProposalTreasury::renounceRoleCall { role, account }.abi_encode(),
            DecodedProposalAction::TreasuryRenounceRole { role, account },
        ),
    ];
    let arbitrum_treasury =
        railgun_ui::governance_treasury(42161).expect("arbitrum treasury metadata");
    assert_eq!(
        decode_proposal_action(42161, arbitrum_treasury, &calls[0].0, None, None),
        Some(calls[0].1.clone())
    );
    for (calldata, expected) in calls {
        assert_eq!(
            decode_proposal_action(1, treasury, &calldata, None, None),
            Some(expected)
        );
        assert_eq!(
            decode_proposal_action(1, other, &calldata, None, None),
            None
        );
    }
}

#[test]
fn proposal_role_labels_cover_known_treasury_roles() {
    assert_eq!(proposal_role_label(B256::ZERO), "DEFAULT_ADMIN_ROLE");
    assert_eq!(
        proposal_role_label(alloy::primitives::keccak256(b"TRANSFER_ROLE")),
        "TRANSFER_ROLE"
    );
    let unknown = B256::repeat_byte(7);
    assert_eq!(proposal_role_label(unknown), unknown.to_string());
}

#[test]
fn proposal_decodes_sender_writes_at_any_target_and_labels_decoded_actions() {
    let first_target = address!("0x1111111111111111111111111111111111111111");
    let second_target = address!("0x2222222222222222222222222222222222222222");
    let executor = address!("0x3333333333333333333333333333333333333333");
    let empty_registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let ready_task = ProposalOpStackSender::readyTaskCall {
        taskId: U256::from(1),
    }
    .abi_encode();
    let ready_task_decoded = DecodedProposalAction::OpStackReadyTask {
        task_id: U256::from(1),
    };
    assert_eq!(
        decode_proposal_action(1, first_target, &ready_task, None, None),
        Some(ready_task_decoded.clone())
    );
    assert_eq!(
        proposal_action_target_label(
            1,
            first_target,
            &empty_registry,
            None,
            Some(&ready_task_decoded),
        ),
        Some("OpStack sender".to_owned())
    );

    let treasury = railgun_ui::governance_treasury(1).expect("mainnet treasury metadata");
    assert_eq!(
        proposal_action_target_label(
            1,
            treasury,
            &empty_registry,
            None,
            Some(&ready_task_decoded),
        ),
        Some("Treasury".to_owned())
    );

    let delegator = railgun_ui::governance_contracts(1)
        .expect("mainnet governance metadata")
        .delegator;
    assert_eq!(
        proposal_action_target_label(
            1,
            delegator,
            &empty_registry,
            None,
            Some(&ready_task_decoded),
        ),
        Some("Delegator".to_owned())
    );

    let set_executor = ProposalOpStackSender::setExecutorL2Call { executor }.abi_encode();
    let set_executor_decoded = DecodedProposalAction::OpStackSetExecutorL2 { executor };
    assert_eq!(
        decode_proposal_action(42161, second_target, &set_executor, None, None),
        Some(set_executor_decoded.clone())
    );
    assert_eq!(
        proposal_action_target_label(
            42161,
            second_target,
            &empty_registry,
            None,
            Some(&set_executor_decoded),
        ),
        Some("OpStack sender".to_owned())
    );

    let transfer_ownership =
        ProposalOwnable::transferOwnershipCall { newOwner: executor }.abi_encode();
    assert_eq!(&transfer_ownership[..4], &[0xf2, 0xfd, 0xe3, 0x8b]);
    assert_eq!(
        decode_proposal_action(1, first_target, &transfer_ownership, None, None),
        Some(DecodedProposalAction::TransferOwnership {
            new_owner: executor
        })
    );
    assert_eq!(
        proposal_action_target_label(
            1,
            first_target,
            &empty_registry,
            None,
            Some(&DecodedProposalAction::TransferOwnership {
                new_owner: executor
            }),
        ),
        None
    );
    let renounce_ownership = ProposalOwnable::renounceOwnershipCall {}.abi_encode();
    assert_eq!(
        decode_proposal_action(42161, second_target, &renounce_ownership, None, None),
        None
    );
}

#[test]
fn proposal_decodes_wrapped_native_calls_only_at_configured_target() {
    let wrapped = address!("0x1111111111111111111111111111111111111111");
    let other = address!("0x2222222222222222222222222222222222222222");
    let deposit = ProposalWrappedNative::depositCall {}.abi_encode();
    assert_eq!(
        decode_proposal_action(1, wrapped, &deposit, Some(wrapped), None),
        Some(DecodedProposalAction::WrappedDeposit)
    );
    assert_eq!(
        decode_proposal_action(1, other, &deposit, Some(wrapped), None),
        None
    );
    let amount = U256::from(9);
    let withdraw = ProposalWrappedNative::withdrawCall { amount }.abi_encode();
    assert_eq!(
        decode_proposal_action(1, wrapped, &withdraw, Some(wrapped), None),
        Some(DecodedProposalAction::WrappedWithdraw { amount })
    );
}

#[test]
fn proposal_keeps_unknown_malformed_and_trailing_calls_undecoded() {
    let target = address!("0x1111111111111111111111111111111111111111");
    let mut trailing = ProposalErc20::transferCall {
        recipient: target,
        amount: U256::from(1),
    }
    .abi_encode();
    trailing.push(0);
    assert_eq!(
        decode_proposal_action(1, target, &trailing, None, None),
        None
    );
    assert_eq!(
        decode_proposal_action(1, target, &[0xa9, 0x05], None, None),
        None
    );
    assert_eq!(
        decode_proposal_action(1, target, &[0xff; 36], None, None),
        None
    );
}
