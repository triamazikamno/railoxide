use super::*;

impl DecodedProposalAction {
    pub(super) const fn method_name(&self) -> &'static str {
        match self {
            Self::Erc20Transfer { .. } => "transfer",
            Self::Erc20TransferFrom { .. } => "transferFrom",
            Self::Erc20Approve { .. } => "approve",
            Self::TreasuryTransferErc20 { .. } => "transferERC20",
            Self::TreasuryTransferEth { .. } => "transferETH",
            Self::TreasuryInitialize { .. } => "initializeTreasury",
            Self::TreasuryGrantRole { .. } => "grantRole",
            Self::TreasuryRevokeRole { .. } => "revokeRole",
            Self::TreasuryRenounceRole { .. } => "renounceRole",
            Self::OpStackReadyTask { .. } => "readyTask",
            Self::OpStackSetExecutorL2 { .. } => "setExecutorL2",
            Self::TransferOwnership { .. } => "transferOwnership",
            Self::WrappedDeposit => "deposit",
            Self::WrappedWithdraw { .. } => "withdraw",
            Self::ProxyUpgrade { .. } => "upgrade",
            Self::ProxyPause { .. } => "pause",
            Self::ProxyTransferOwnership { .. } => "transferProxyOwnership",
            Self::ChangeFee { .. } => "changeFee",
            Self::GovernanceMint { .. } => "governanceMint",
            Self::SetIntervalBP { .. } => "setIntervalBP",
            Self::AddTokens { .. } => "addTokens",
            Self::DelegatorSetPermission { .. } => "setPermission",
        }
    }

    const fn contract_family_label(&self) -> Option<&'static str> {
        match self {
            Self::OpStackReadyTask { .. } | Self::OpStackSetExecutorL2 { .. } => {
                Some("OpStack sender")
            }
            Self::ProxyUpgrade { .. }
            | Self::ProxyPause { .. }
            | Self::ProxyTransferOwnership { .. } => Some("Proxy admin"),
            _ => None,
        }
    }
}

fn decode_exact_call<C: SolCall>(calldata: &[u8]) -> Option<C> {
    let call = C::abi_decode_validate(calldata).ok()?;
    (call.abi_encode().as_slice() == calldata).then_some(call)
}

pub(super) fn decode_proposal_action(
    chain_id: u64,
    target: Address,
    calldata: &[u8],
    wrapped_native_token: Option<Address>,
    railgun_contract: Option<Address>,
) -> Option<DecodedProposalAction> {
    if railgun_ui::governance_treasury(chain_id).is_some_and(|treasury| treasury == target) {
        if let Some(call) = decode_exact_call::<ProposalTreasury::transferERC20Call>(calldata) {
            return Some(DecodedProposalAction::TreasuryTransferErc20 {
                token: call.token,
                to: call.to,
                amount: call.amount,
            });
        }
        if let Some(call) = decode_exact_call::<ProposalTreasury::transferETHCall>(calldata) {
            return Some(DecodedProposalAction::TreasuryTransferEth {
                to: call.to,
                amount: call.amount,
            });
        }
        if let Some(call) = decode_exact_call::<ProposalTreasury::initializeTreasuryCall>(calldata)
        {
            return Some(DecodedProposalAction::TreasuryInitialize { owner: call.owner });
        }
        if let Some(call) = decode_exact_call::<ProposalTreasury::grantRoleCall>(calldata) {
            return Some(DecodedProposalAction::TreasuryGrantRole {
                role: call.role,
                account: call.account,
            });
        }
        if let Some(call) = decode_exact_call::<ProposalTreasury::revokeRoleCall>(calldata) {
            return Some(DecodedProposalAction::TreasuryRevokeRole {
                role: call.role,
                account: call.account,
            });
        }
        if let Some(call) = decode_exact_call::<ProposalTreasury::renounceRoleCall>(calldata) {
            return Some(DecodedProposalAction::TreasuryRenounceRole {
                role: call.role,
                account: call.account,
            });
        }
    }

    if let Some(call) = decode_exact_call::<ProposalOpStackSender::readyTaskCall>(calldata) {
        return Some(DecodedProposalAction::OpStackReadyTask {
            task_id: call.taskId,
        });
    }
    if let Some(call) = decode_exact_call::<ProposalOpStackSender::setExecutorL2Call>(calldata) {
        return Some(DecodedProposalAction::OpStackSetExecutorL2 {
            executor: call.executor,
        });
    }

    if let Some(call) = decode_exact_call::<ProposalOwnable::transferOwnershipCall>(calldata) {
        return Some(DecodedProposalAction::TransferOwnership {
            new_owner: call.newOwner,
        });
    }

    if let Some(call) = decode_exact_call::<ProposalProxyAdmin::upgradeCall>(calldata) {
        return Some(DecodedProposalAction::ProxyUpgrade {
            proxy: call.proxy,
            implementation: call.implementation,
        });
    }
    if let Some(call) = decode_exact_call::<ProposalProxyAdmin::pauseCall>(calldata) {
        return Some(DecodedProposalAction::ProxyPause { proxy: call.proxy });
    }
    if let Some(call) =
        decode_exact_call::<ProposalProxyAdmin::transferProxyOwnershipCall>(calldata)
    {
        return Some(DecodedProposalAction::ProxyTransferOwnership {
            proxy: call.proxy,
            new_owner: call.newOwner,
        });
    }

    if railgun_contract.is_some_and(|railgun| railgun == target)
        && let Some(call) = decode_exact_call::<ProposalRailgun::changeFeeCall>(calldata)
    {
        return Some(DecodedProposalAction::ChangeFee {
            shield_fee: U256::from(call.shieldFee),
            unshield_fee: U256::from(call.unshieldFee),
            nft_fee: call.nftFee,
        });
    }

    if railgun_ui::governance_contracts(chain_id)
        .is_some_and(|contracts| contracts.governance_token == target)
        && let Some(call) =
            decode_exact_call::<ProposalGovernanceToken::governanceMintCall>(calldata)
    {
        return Some(DecodedProposalAction::GovernanceMint {
            account: call.account,
            amount: call.amount,
        });
    }

    if railgun_ui::governance_contracts(chain_id)
        .is_some_and(|contracts| contracts.governor_rewards == target)
        && let Some(call) =
            decode_exact_call::<ProposalGovernorRewards::setIntervalBPCall>(calldata)
    {
        return Some(DecodedProposalAction::SetIntervalBP {
            new_interval_bp: call.newIntervalBP,
        });
    }
    if railgun_ui::governance_contracts(chain_id)
        .is_some_and(|contracts| contracts.governor_rewards == target)
        && let Some(call) = decode_exact_call::<ProposalGovernorRewards::addTokensCall>(calldata)
    {
        return Some(DecodedProposalAction::AddTokens {
            tokens: call.tokens,
        });
    }

    if railgun_ui::governance_contracts(chain_id)
        .is_some_and(|contracts| contracts.delegator == target)
        && let Some(call) = decode_exact_call::<ProposalDelegator::setPermissionCall>(calldata)
    {
        return Some(DecodedProposalAction::DelegatorSetPermission {
            caller: call.caller,
            contract_address: call.contractAddress,
            selector: call.selector,
            permission: call.permission,
        });
    }

    if wrapped_native_token.is_some_and(|wrapped| wrapped == target) {
        if decode_exact_call::<ProposalWrappedNative::depositCall>(calldata).is_some() {
            return Some(DecodedProposalAction::WrappedDeposit);
        }
        if let Some(call) = decode_exact_call::<ProposalWrappedNative::withdrawCall>(calldata) {
            return Some(DecodedProposalAction::WrappedWithdraw {
                amount: call.amount,
            });
        }
    }

    if let Some(call) = decode_exact_call::<ProposalErc20::transferCall>(calldata) {
        return Some(DecodedProposalAction::Erc20Transfer {
            recipient: call.recipient,
            amount: call.amount,
        });
    }
    if let Some(call) = decode_exact_call::<ProposalErc20::transferFromCall>(calldata) {
        return Some(DecodedProposalAction::Erc20TransferFrom {
            from: call.from,
            to: call.to,
            amount: call.amount,
        });
    }
    decode_exact_call::<ProposalErc20::approveCall>(calldata).map(|call| {
        DecodedProposalAction::Erc20Approve {
            spender: call.spender,
            amount: call.amount,
        }
    })
}

pub(super) fn action_calldata_hex(calldata: &alloy::primitives::Bytes) -> String {
    format!("0x{}", alloy::hex::encode(calldata))
}

pub(super) fn compact_calldata_display(value: &str) -> String {
    if value.chars().count() <= 42 {
        return value.to_owned();
    }
    let prefix = value.chars().take(20).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(16)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

pub(super) fn proposal_action_id(
    identity: &ProposalIdentity,
    ordinal: usize,
    field: &str,
    control: &str,
) -> SharedString {
    SharedString::from(format!(
        "wallet-proposal-action-{}-{}-{}-{ordinal}-{field}-{control}",
        proposal_version_label(identity.contract_version),
        identity.contract_address,
        identity.index,
    ))
}

fn proposal_known_address_label(
    chain_id: u64,
    address: Address,
    token_registry: &wallet_ops::settings::EffectiveTokenRegistry,
    effective_chain: Option<&EffectiveChainConfig>,
) -> Option<String> {
    if let Some(metadata) = token_display_metadata(Some(token_registry), chain_id, &address) {
        return Some(metadata.symbol);
    }
    if let Some(contracts) = railgun_ui::governance_contracts(chain_id) {
        if address == contracts.governance_token {
            return Some("Governance token".to_owned());
        }
        if address == contracts.delegator {
            return Some("Delegator".to_owned());
        }
        if address == contracts.voting {
            return Some("Voting".to_owned());
        }
        if contracts.voting_legacy == Some(address) {
            return Some("Legacy voting".to_owned());
        }
        if address == contracts.staking {
            return Some("Staking".to_owned());
        }
        if address == contracts.governor_rewards {
            return Some("Governor rewards".to_owned());
        }
        if let Some(reward) = contracts
            .reward_tokens
            .iter()
            .find(|reward| reward.token == address)
        {
            return Some(reward.symbol.to_owned());
        }
    }
    if railgun_ui::governance_treasury(chain_id) == Some(address) {
        return Some("Treasury".to_owned());
    }
    let effective_chain = effective_chain?;
    let private = effective_chain.railgun.as_ref();
    let configured = [
        (
            "RAILGUN",
            private.map(|private| private.deployment.contract),
        ),
        (
            "Relay Adapt",
            private.and_then(|private| private.deployment.relay_adapt_contract),
        ),
        (
            "Relay Adapt 7702",
            private.map(|private| private.deployment.relay_adapt_7702_contract),
        ),
        ("Multicall", effective_chain.rpc_route.multicall()),
        ("Wrapped native token", effective_chain.wrapped_native_token),
        (
            "Coinbase payer",
            private.and_then(|private| private.coinbase_payer),
        ),
    ];
    configured
        .into_iter()
        .find_map(|(label, target)| (target == Some(address)).then(|| label.to_owned()))
}

pub(super) fn proposal_action_target_label(
    chain_id: u64,
    target: Address,
    token_registry: &wallet_ops::settings::EffectiveTokenRegistry,
    effective_chain: Option<&EffectiveChainConfig>,
    decoded: Option<&DecodedProposalAction>,
) -> Option<String> {
    proposal_known_address_label(chain_id, target, token_registry, effective_chain).or_else(|| {
        decoded
            .and_then(DecodedProposalAction::contract_family_label)
            .map(str::to_owned)
    })
}

pub(super) fn proposal_role_label(role: B256) -> String {
    if role == B256::ZERO {
        "DEFAULT_ADMIN_ROLE".to_owned()
    } else if role == alloy::primitives::keccak256(b"TRANSFER_ROLE") {
        "TRANSFER_ROLE".to_owned()
    } else {
        role.to_string()
    }
}

fn proposal_basis_points_label(basis_points: U256) -> String {
    let whole_percent = basis_points / U256::from(100);
    let fractional_percent = (basis_points % U256::from(100)).to::<u8>();
    format!("{whole_percent}.{fractional_percent:02}% ({basis_points} bp)")
}

fn proposal_party_badge_label(
    chain_id: u64,
    address: Address,
    token_registry: &wallet_ops::settings::EffectiveTokenRegistry,
    effective_chain: Option<&EffectiveChainConfig>,
    public_accounts: &[PublicAccountMetadata],
    public_address_book: Option<&[PublicAddressBookEntry]>,
) -> Option<String> {
    proposal_known_address_label(chain_id, address, token_registry, effective_chain)
        .or_else(|| {
            public_accounts
                .iter()
                .find(|account| account.address == address)
                .map(|account| {
                    account
                        .label
                        .as_deref()
                        .filter(|label| !label.trim().is_empty())
                        .map_or_else(
                            || "Your account".to_owned(),
                            |label| format!("Your account · {label}"),
                        )
                })
        })
        .or_else(|| {
            public_address_book
                .and_then(|entries| entries.iter().find(|entry| entry.address == address))
                .map(|entry| entry.label.clone())
        })
}

fn proposal_party_detail_with_context(
    role: &str,
    chain_id: u64,
    address: Address,
    token_registry: &wallet_ops::settings::EffectiveTokenRegistry,
    effective_chain: Option<&EffectiveChainConfig>,
    public_accounts: &[PublicAccountMetadata],
    public_address_book: Option<&[PublicAddressBookEntry]>,
) -> ProposalDecodedDetail {
    ProposalDecodedDetail::Party {
        role: role.to_owned(),
        address,
        copy_value: address.to_checksum(None),
        badge: proposal_party_badge_label(
            chain_id,
            address,
            token_registry,
            effective_chain,
            public_accounts,
            public_address_book,
        ),
    }
}

fn proposal_delegator_selector_label(selector: FixedBytes<4>) -> String {
    let raw = format!("0x{}", alloy::hex::encode(selector.as_slice()));
    if selector == FixedBytes::from([0; 4]) {
        format!("Any function · {raw}")
    } else if selector == FixedBytes::from([0x2e, 0xc0, 0xf3, 0x59]) {
        format!("setVerificationKey · {raw}")
    } else {
        raw
    }
}

pub(super) fn proposal_decoded_preview(
    decoded: &DecodedProposalAction,
    target: Address,
    action_value: U256,
    chain_id: u64,
    anchor_rates: &TokenAnchorRateCache,
    token_registry: &wallet_ops::settings::EffectiveTokenRegistry,
    effective_chain: Option<&EffectiveChainConfig>,
    public_accounts: &[PublicAccountMetadata],
    public_address_book: Option<&[PublicAddressBookEntry]>,
) -> ProposalDecodedPreview {
    let party = |role: &str, address: Address| {
        proposal_party_detail_with_context(
            role,
            chain_id,
            address,
            token_registry,
            effective_chain,
            public_accounts,
            public_address_book,
        )
    };
    let combine_context = |usd_context: Option<String>, semantic_context: Option<String>| match (
        usd_context,
        semantic_context,
    ) {
        (Some(usd), Some(context)) => Some(format!("{usd} · {context}")),
        (Some(usd), None) | (None, Some(usd)) => Some(usd),
        (None, None) => None,
    };
    let token_hero = |token: Address, amount: U256, context: Option<String>| {
        if let Some(metadata) = token_display_metadata(Some(token_registry), chain_id, &token) {
            let usd_context = anchor_rates
                .cached_token_usd_micro_value(chain_id, token, amount)
                .and_then(|usd| {
                    railgun_ui::non_redundant_usd_micro_value(amount, metadata.decimals, usd)
                })
                .map(|usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)));
            ProposalDecodedHero {
                rounded: format_token_amount_for_display(
                    chain_id,
                    token,
                    amount,
                    Some(token_registry),
                ),
                icon: metadata.icon_path,
                context: combine_context(usd_context, context),
                amount_copy_value: None,
                context_copy_value: None,
                monospace: false,
                danger: false,
            }
        } else {
            ProposalDecodedHero {
                rounded: format!("{amount} raw token units"),
                icon: None,
                context: Some(format!(
                    "Token not in registry · {}",
                    railgun_ui::short_address(&token)
                )),
                amount_copy_value: Some(amount.to_string()),
                context_copy_value: Some(token.to_checksum(None)),
                monospace: true,
                danger: false,
            }
        }
    };
    let native_hero = |amount: U256, context: Option<String>| {
        let usd_context = anchor_rates
            .cached_native_usd_micro_value(chain_id, amount)
            .map(|usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)));
        ProposalDecodedHero {
            rounded: format_native_token_amount_for_display(chain_id, amount),
            icon: railgun_ui::native_currency_icon_asset_path(chain_id)
                .map(WalletIconSource::embedded),
            context: combine_context(usd_context, context),
            amount_copy_value: None,
            context_copy_value: None,
            monospace: false,
            danger: false,
        }
    };
    match decoded {
        DecodedProposalAction::Erc20Transfer { recipient, amount } => ProposalDecodedPreview {
            verb: "Send".to_owned(),
            hero: Some(token_hero(target, *amount, None)),
            details: vec![party("TO", *recipient)],
            unlimited_warning: None,
        },
        DecodedProposalAction::Erc20TransferFrom { from, to, amount } => ProposalDecodedPreview {
            verb: "Send".to_owned(),
            hero: Some(token_hero(target, *amount, None)),
            details: vec![
                party("FROM", *from),
                ProposalDecodedDetail::Connector,
                party("TO", *to),
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::Erc20Approve { spender, amount } => {
            let unlimited = *amount == U256::MAX;
            let mut hero = token_hero(target, *amount, Some("allowance".to_owned()));
            if unlimited {
                hero.rounded = token_display_metadata(Some(token_registry), chain_id, &target)
                    .map_or_else(
                        || "Unlimited raw token units".to_owned(),
                        |metadata| format!("Unlimited {}", metadata.symbol),
                    );
                hero.danger = true;
            }
            ProposalDecodedPreview {
                verb: "Allow spending".to_owned(),
                hero: Some(hero),
                details: vec![party("SPENDER", *spender)],
                unlimited_warning: unlimited.then(|| {
                    format!(
                        "Unlimited allowance: {} can keep spending this token until the allowance is revoked.",
                        spender.to_checksum(None)
                    )
                }),
            }
        }
        DecodedProposalAction::TreasuryTransferErc20 { token, to, amount } => {
            ProposalDecodedPreview {
                verb: "Send from treasury".to_owned(),
                hero: Some(token_hero(*token, *amount, None)),
                details: vec![
                    party("FROM", target),
                    ProposalDecodedDetail::Connector,
                    party("TO", *to),
                ],
                unlimited_warning: None,
            }
        }
        DecodedProposalAction::TreasuryTransferEth { to, amount } => ProposalDecodedPreview {
            verb: "Send from treasury".to_owned(),
            hero: Some(native_hero(*amount, None)),
            details: vec![
                party("FROM", target),
                ProposalDecodedDetail::Connector,
                party("TO", *to),
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::TreasuryInitialize { owner } => ProposalDecodedPreview {
            verb: "Initialize treasury".to_owned(),
            hero: None,
            details: vec![party("OWNER", *owner)],
            unlimited_warning: None,
        },
        DecodedProposalAction::TreasuryGrantRole { role, account } => ProposalDecodedPreview {
            verb: "Grant role".to_owned(),
            hero: None,
            details: vec![
                ProposalDecodedDetail::Value {
                    role: "ROLE".to_owned(),
                    value: proposal_role_label(*role),
                    copy_value: None,
                    monospace: true,
                },
                party("ACCOUNT", *account),
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::TreasuryRevokeRole { role, account } => ProposalDecodedPreview {
            verb: "Revoke role".to_owned(),
            hero: None,
            details: vec![
                ProposalDecodedDetail::Value {
                    role: "ROLE".to_owned(),
                    value: proposal_role_label(*role),
                    copy_value: None,
                    monospace: true,
                },
                party("ACCOUNT", *account),
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::TreasuryRenounceRole { role, account } => ProposalDecodedPreview {
            verb: "Renounce role".to_owned(),
            hero: None,
            details: vec![
                ProposalDecodedDetail::Value {
                    role: "ROLE".to_owned(),
                    value: proposal_role_label(*role),
                    copy_value: None,
                    monospace: true,
                },
                party("ACCOUNT", *account),
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::OpStackReadyTask { task_id } => ProposalDecodedPreview {
            verb: "Ready task".to_owned(),
            hero: None,
            details: vec![ProposalDecodedDetail::Value {
                role: "TASK ID".to_owned(),
                value: task_id.to_string(),
                copy_value: None,
                monospace: true,
            }],
            unlimited_warning: None,
        },
        DecodedProposalAction::OpStackSetExecutorL2 { executor } => ProposalDecodedPreview {
            verb: "Set executor".to_owned(),
            hero: None,
            details: vec![party("EXECUTOR", *executor)],
            unlimited_warning: None,
        },
        DecodedProposalAction::TransferOwnership { new_owner } => ProposalDecodedPreview {
            verb: "Transfer ownership".to_owned(),
            hero: None,
            details: vec![party("NEW OWNER", *new_owner)],
            unlimited_warning: None,
        },
        DecodedProposalAction::WrappedDeposit => ProposalDecodedPreview {
            verb: "Wrap".to_owned(),
            hero: Some(native_hero(
                action_value,
                native_wrapped_output_labels(chain_id)
                    .map(|(_, wrapped)| format!("wrapped into {wrapped}")),
            )),
            details: Vec::new(),
            unlimited_warning: None,
        },
        DecodedProposalAction::WrappedWithdraw { amount } => ProposalDecodedPreview {
            verb: "Unwrap".to_owned(),
            hero: Some(token_hero(
                target,
                *amount,
                native_wrapped_output_labels(chain_id)
                    .map(|(native, _)| format!("unwrapped into {native}")),
            )),
            details: Vec::new(),
            unlimited_warning: None,
        },
        DecodedProposalAction::ProxyUpgrade {
            proxy,
            implementation,
        } => ProposalDecodedPreview {
            verb: "Upgrade proxy".to_owned(),
            hero: None,
            details: vec![
                party("PROXY", *proxy),
                party("IMPLEMENTATION", *implementation),
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::ProxyPause { proxy } => ProposalDecodedPreview {
            verb: "Pause proxy".to_owned(),
            hero: None,
            details: vec![party("PROXY", *proxy)],
            unlimited_warning: None,
        },
        DecodedProposalAction::ProxyTransferOwnership { proxy, new_owner } => {
            ProposalDecodedPreview {
                verb: "Transfer proxy ownership".to_owned(),
                hero: None,
                details: vec![party("PROXY", *proxy), party("NEW OWNER", *new_owner)],
                unlimited_warning: None,
            }
        }
        DecodedProposalAction::ChangeFee {
            shield_fee,
            unshield_fee,
            nft_fee,
        } => ProposalDecodedPreview {
            verb: "Change protocol fees".to_owned(),
            hero: None,
            details: vec![
                ProposalDecodedDetail::Value {
                    role: "SHIELD FEE".to_owned(),
                    value: proposal_basis_points_label(*shield_fee),
                    copy_value: Some(shield_fee.to_string()),
                    monospace: false,
                },
                ProposalDecodedDetail::Value {
                    role: "UNSHIELD FEE".to_owned(),
                    value: proposal_basis_points_label(*unshield_fee),
                    copy_value: Some(unshield_fee.to_string()),
                    monospace: false,
                },
                ProposalDecodedDetail::Value {
                    role: "NFT FEE".to_owned(),
                    value: format!(
                        "{} ({} wei)",
                        format_native_token_amount_for_display(chain_id, *nft_fee),
                        nft_fee
                    ),
                    copy_value: Some(nft_fee.to_string()),
                    monospace: false,
                },
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::GovernanceMint { account, amount } => ProposalDecodedPreview {
            verb: "Mint governance tokens".to_owned(),
            hero: Some(token_hero(target, *amount, None)),
            details: vec![party("TO", *account)],
            unlimited_warning: None,
        },
        DecodedProposalAction::SetIntervalBP { new_interval_bp } => ProposalDecodedPreview {
            verb: "Set reward interval rate".to_owned(),
            hero: None,
            details: vec![ProposalDecodedDetail::Value {
                role: "RATE".to_owned(),
                value: proposal_basis_points_label(*new_interval_bp),
                copy_value: Some(new_interval_bp.to_string()),
                monospace: false,
            }],
            unlimited_warning: None,
        },
        DecodedProposalAction::AddTokens { tokens } => ProposalDecodedPreview {
            verb: "Add reward tokens".to_owned(),
            hero: None,
            details: tokens.iter().map(|token| party("TOKEN", *token)).collect(),
            unlimited_warning: None,
        },
        DecodedProposalAction::DelegatorSetPermission {
            caller,
            contract_address,
            selector,
            permission,
        } => ProposalDecodedPreview {
            verb: if *permission {
                "Grant call permission".to_owned()
            } else {
                "Revoke call permission".to_owned()
            },
            hero: None,
            details: vec![
                party("CALLER", *caller),
                if *contract_address == Address::ZERO {
                    ProposalDecodedDetail::Value {
                        role: "CONTRACT".to_owned(),
                        value: format!("Any contract · {}", short_address(contract_address)),
                        copy_value: Some(contract_address.to_checksum(None)),
                        monospace: false,
                    }
                } else {
                    party("CONTRACT", *contract_address)
                },
                ProposalDecodedDetail::Value {
                    role: "FUNCTION".to_owned(),
                    value: proposal_delegator_selector_label(*selector),
                    copy_value: Some(format!("0x{}", alloy::hex::encode(selector.as_slice()))),
                    monospace: true,
                },
            ],
            unlimited_warning: None,
        },
    }
}
