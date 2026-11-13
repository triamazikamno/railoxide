use super::*;

#[test]
fn balance_plan_batches_native_and_known_tokens_per_account() {
    let account = PublicAccountMetadata {
        public_account_uuid: "public-1".to_string(),
        address: address!("0x1111111111111111111111111111111111111111"),
        label: None,
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: "wallet-1".to_string(),
        },
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let accounts = [
        account.clone(),
        PublicAccountMetadata {
            public_account_uuid: "public-2".to_string(),
            address: address!("0x2222222222222222222222222222222222222222"),
            ..account.clone()
        },
        PublicAccountMetadata {
            public_account_uuid: "public-3".to_string(),
            address: address!("0x3333333333333333333333333333333333333333"),
            ..account.clone()
        },
    ];
    let requested = [accounts[0].clone(), accounts[2].clone()];
    let calls = plan_public_balance_calls(
        &effective_chain_for_rpc("http://127.0.0.1:1", 0),
        &requested,
        None,
        BlockId::latest(),
    )
    .unwrap();
    // Only the two requested accounts of the three are read.
    let read_accounts: std::collections::BTreeSet<_> = calls
        .iter()
        .map(|call| call.public_account_uuid.as_str())
        .collect();
    assert_eq!(
        read_accounts,
        std::collections::BTreeSet::from(["public-1", "public-3"])
    );

    let native = calls.first().expect("native call");
    assert_eq!(native.asset.id, PublicAssetId::Native);
    assert_eq!(native.read, RpcRead::get_balance(account.address));
    let token = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let erc20 = calls
        .iter()
        .find(|call| call.asset.id == PublicAssetId::Erc20(token))
        .expect("known token call");
    assert_eq!(
        erc20.read,
        RpcRead::eth_call(
            token,
            PublicErc20::balanceOfCall {
                account: account.address,
            }
            .abi_encode()
            .into(),
        )
    );
}

#[tokio::test]
async fn balance_refresh_submits_more_wallet_reads_than_the_dapp_cap() {
    let accounts = (1_u8..=3)
        .map(|index| PublicAccountMetadata {
            public_account_uuid: format!("public-{index}"),
            address: alloy::primitives::Address::from([index; 20]),
            label: None,
            source: PublicAccountSource::Derived,
            scope: PublicAccountScope::PrivateWallet {
                wallet_uuid: "wallet-1".to_string(),
            },
            derivation_index: Some(u32::from(index)),
            hardware_descriptor: None,
            status: PublicAccountStatus::Active,
            display_order: u32::from(index),
        })
        .collect::<Vec<_>>();
    let planned = plan_public_balance_calls(
        &effective_chain_for_rpc("http://127.0.0.1:1", 0),
        &accounts,
        None,
        BlockId::latest(),
    )
    .unwrap();
    assert!(
        planned.len() > 64,
        "a three-account chain 1 refresh must exceed the dapp read cap, planned {}",
        planned.len()
    );

    let (broker, _executions) = crate::rpc_broker::tests::spawn_counting_test_broker();
    let route = RpcRoute::from(sensitive(&RpcChainRoute::new(
        1,
        vec![Url::parse("https://balances.invalid").expect("route URL")],
    )));
    let results = broker
        .submit(RpcSubmission::new(
            route,
            planned.iter().map(|call| call.read.clone()).collect(),
            WalletRpcOrigin::PublicWallet.into(),
        ))
        .await
        .expect("wallet balance submission is admitted");
    assert_eq!(results.len(), planned.len());
    assert!(results.iter().all(std::result::Result::is_ok));
}

fn balance_word_for_test(value: u64) -> Bytes {
    Bytes::from(U256::from(value).to_be_bytes::<32>().to_vec())
}

fn aggregated_calls_for_test(request: &Value) -> Vec<IMulticall3::Call> {
    let input = alloy::hex::decode(request["params"][0]["input"].as_str().unwrap()).unwrap();
    IMulticall3::tryAggregateCall::abi_decode(&input)
        .expect("one aggregate per account")
        .calls
}

#[tokio::test]
async fn balance_refresh_pins_one_account_to_its_route_block_number() {
    const BLOCK: u64 = 10;
    enum Case {
        Accepted,
        BelowMinimum,
        FailedToken,
        // The first two-member aggregate answers -32000, so the broker splits it.
        Reduced,
    }
    let account = PublicAccountMetadata {
        public_account_uuid: "public-1".to_string(),
        address: Address::repeat_byte(1),
        label: None,
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: "wallet-1".to_string(),
        },
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let token = Address::repeat_byte(2);
    let registry = crate::settings::EffectiveTokenRegistry {
        tokens: [(
            (1, token.to_string()),
            crate::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: token.to_string(),
                symbol: "TEST".to_string(),
                decimals: 18,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]
        .into(),
    };
    for case in [
        Case::Accepted,
        Case::BelowMinimum,
        Case::FailedToken,
        Case::Reduced,
    ] {
        let minimum = match case {
            Case::Accepted => Some(BLOCK),
            Case::BelowMinimum => Some(BLOCK + 1),
            Case::FailedToken | Case::Reduced => None,
        };
        let fail_token = matches!(case, Case::FailedToken);
        let reduce = Arc::new(AtomicBool::new(matches!(case, Case::Reduced)));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request| {
                recorded.lock().unwrap().push(request.clone());
                if request["method"] == "eth_blockNumber" {
                    return json!({"jsonrpc": "2.0", "id": request["id"], "result": "0xa"});
                }
                let members = aggregated_calls_for_test(&request);
                if members.len() > 1 && reduce.swap(false, Ordering::SeqCst) {
                    return json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "too large"}});
                }
                let returns = members
                    .into_iter()
                    .map(|call| {
                        let (success, data) = if call
                            .callData
                            .starts_with(&IMulticall3::getEthBalanceCall::SELECTOR)
                        {
                            (true, balance_word_for_test(7))
                        } else if fail_token {
                            (false, Bytes::new())
                        } else {
                            (true, balance_word_for_test(9))
                        };
                        IMulticall3::Result {
                            success,
                            returnData: data,
                        }
                    })
                    .collect::<Vec<_>>();
                let result =
                    Bytes::from(IMulticall3::tryAggregateCall::abi_encode_returns(&returns));
                json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
            }),
            Arc::default(),
            Arc::default(),
        )
        .await;
        let chain = effective_chain_for_rpc(endpoint.as_str(), 0);
        let http = HttpContext::direct_for_tests();
        let started = Instant::now();
        let result =
            refresh_public_balances_at_least(1, &account, &chain, Some(&registry), &http, minimum)
                .await;
        let calls = calls.lock().unwrap();
        // The route's block number comes first, then the balances pinned to it.
        assert_eq!(calls[0]["method"], "eth_blockNumber");
        let balance_calls = &calls[1..];
        for call in balance_calls {
            assert_eq!(call["method"], "eth_call");
            assert_eq!(call["params"][1], json!("0xa"), "pinned to block {BLOCK}");
            assert!(aggregated_calls_for_test(call).iter().all(|member| {
                !member
                    .callData
                    .starts_with(&IMulticall3::getBlockNumberCall::SELECTOR)
            }));
        }
        match case {
            Case::BelowMinimum => {
                assert!(result.is_err(), "a number below the minimum is not current");
                assert!(balance_calls.is_empty(), "no balance read is sent");
            }
            Case::Accepted | Case::Reduced => {
                if matches!(case, Case::Reduced) {
                    assert!(balance_calls.len() > 1, "the aggregate was split");
                } else {
                    assert_eq!(balance_calls.len(), 1);
                    assert_eq!(aggregated_calls_for_test(&balance_calls[0]).len(), 2);
                }
                let snapshot = result.unwrap();
                let row = &snapshot.accounts[0];
                assert_eq!(row.balances.len(), 2);
                assert!(
                    row.balances
                        .iter()
                        .all(|balance| balance.amount.amount().is_some())
                );
                assert_eq!(row.observed_block, Some(BLOCK));
                assert!(row.observed_at.unwrap() >= started);
            }
            Case::FailedToken => {
                // A failed balance member keeps the balances without an observation.
                assert_eq!(balance_calls.len(), 1);
                let snapshot = result.unwrap();
                let row = &snapshot.accounts[0];
                assert_eq!(row.balances.len(), 2);
                assert_eq!(
                    row.balances
                        .iter()
                        .filter(|balance| balance.amount.amount().is_some())
                        .count(),
                    1
                );
                assert_eq!(row.observed_block, None);
                assert_eq!(row.observed_at, None);
            }
        }
        server.abort();
    }

    // Without an isolated route the account fails and nothing goes over the shared one.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request| {
            recorded.lock().unwrap().push(request.clone());
            json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x"})
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let chain = effective_chain_for_rpc(endpoint.as_str(), 0);
    let http = http_context_for_route(WalletNetworkMode::Tor, "tor");
    assert!(
        refresh_public_balances_at_least(1, &account, &chain, Some(&registry), &http, None)
            .await
            .is_err()
    );
    assert!(calls.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn custom_balance_refresh_preserves_partial_results_without_multicall() {
    const CHAIN: u64 = 999_999;
    let account = PublicAccountMetadata {
        public_account_uuid: "public-1".to_string(),
        address: address!("1111111111111111111111111111111111111111"),
        label: None,
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: "wallet-1".to_string(),
        },
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let tokens = [
        Address::from([1; 20]),
        Address::from([2; 20]),
        Address::from([3; 20]),
    ];
    let registry = crate::settings::EffectiveTokenRegistry {
        tokens: tokens
            .into_iter()
            .map(|token| {
                let address = token.to_string();
                (
                    (CHAIN, address.clone()),
                    crate::settings::EffectiveTokenInfo {
                        chain_id: CHAIN,
                        token_address: address,
                        symbol: "TEST".to_string(),
                        decimals: 18,
                        icon_path: None,
                        price_anchor: None,
                        built_in: false,
                    },
                )
            })
            .collect(),
    };
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request| {
            let result = if request["method"] == "eth_chainId" {
                json!(alloy::primitives::U64::from(CHAIN))
            } else if request["method"] == "eth_blockNumber" {
                json!("0xa")
            } else if request["params"][1] != json!("0xa") {
                // Each individual read must be pinned to the route's block number.
                return json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": "unpinned"}});
            } else if request["method"] == "eth_getBalance" {
                json!("0x7")
            } else {
                let transaction: TransactionRequest = serde_json::from_value(request["params"][0].clone()).unwrap();
                match transaction.to {
                    Some(TxKind::Call(token)) if token == tokens[0] => json!(Bytes::copy_from_slice(&U256::from(9).to_be_bytes::<32>())),
                    Some(TxKind::Call(token)) if token == tokens[1] => json!("0x09"),
                    _ => return json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": "unavailable"}}),
                }
            };
            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        }),
        Arc::default(),
        Arc::default(),
    ).await;
    let mut settings = crate::settings::WalletSettings::default();
    settings.chains.custom.insert(
        CHAIN,
        crate::settings::CustomChainSettings {
            native_usd_pricing: crate::settings::NativeUsdPricing::Default,
            name: "Custom test network".into(),
            native_currency: crate::settings::NativeCurrency {
                name: "Custom coin".into(),
                symbol: "CUSTOM".into(),
                decimals: 6,
            },
            rpc_endpoints: vec![endpoint.to_string().into()],
            explorer_urls: vec![],
            enabled: true,
            contracts: crate::settings::ChainContractSettings::default(),
            finality_depth: None,
            gas: crate::settings::ChainGasSettings::default(),
        },
    );
    let chain = crate::settings::build_effective_chain_configs(&settings)
        .unwrap()
        .get(CHAIN)
        .cloned()
        .unwrap();
    assert!(chain.railgun.is_none());
    assert!(chain.rpc_route.multicall().is_none());
    let http = HttpContext::direct_for_tests();
    let snapshot = refresh_public_balances(CHAIN, &account, &chain, Some(&registry), &http)
        .await
        .unwrap();
    let balances = &snapshot.accounts[0].balances;
    let native = &balances
        .iter()
        .find(|balance| balance.asset.id == PublicAssetId::Native)
        .unwrap()
        .asset;
    assert_eq!(native.symbol, "CUSTOM");
    assert_eq!(native.decimals, 6);
    let amount = |asset| {
        &balances
            .iter()
            .find(|balance| balance.asset.id == asset)
            .unwrap()
            .amount
    };
    assert_eq!(
        amount(PublicAssetId::Native),
        &PublicBalanceAmount::Available(U256::from(7))
    );
    assert_eq!(
        amount(PublicAssetId::Erc20(tokens[0])),
        &PublicBalanceAmount::Available(U256::from(9))
    );
    for token in &tokens[1..] {
        assert_eq!(
            amount(PublicAssetId::Erc20(*token)),
            &PublicBalanceAmount::Unavailable
        );
    }
    server.abort();
}

#[test]
fn balance_assets_use_effective_token_registry_overlays() {
    let mut settings = crate::settings::WalletSettings::default();
    settings
        .tokens
        .built_in_tombstones
        .push(crate::settings::TokenKey {
            chain_id: 1,
            token_address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
        });
    settings
        .tokens
        .custom_tokens
        .push(crate::settings::CustomTokenSettings {
            chain_id: 1,
            token_address: "0x0000000000000000000000000000000000000002".to_string(),
            symbol: "CSTM".to_string(),
            decimals: 9,
            icon_path: None,
            price_anchor: None,
        });
    let registry = crate::settings::build_effective_token_registry(&settings)
        .expect("effective token registry");

    let assets = public_balance_assets_for_chain_with_registry(
        &effective_chain_for_rpc("http://127.0.0.1:1", 0),
        Some(&registry),
    );

    assert!(assets.iter().any(|asset| asset.id == PublicAssetId::Native));
    assert!(!assets.iter().any(|asset| {
        asset.id == PublicAssetId::Erc20(address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"))
    }));
    let custom = assets
        .iter()
        .find(|asset| {
            asset.id == PublicAssetId::Erc20(address!("0x0000000000000000000000000000000000000002"))
        })
        .expect("custom token asset");
    assert_eq!(custom.symbol, "CSTM");
    assert_eq!(custom.decimals, 9);
}

#[test]
fn balance_snapshot_preserves_partial_success() {
    let account = PublicAccountMetadata {
        public_account_uuid: "public-1".to_string(),
        address: address!("0x1111111111111111111111111111111111111111"),
        label: None,
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: "wallet-1".to_string(),
        },
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let mut successful_account = account.clone();
    successful_account.public_account_uuid = "public-2".to_string();
    let mut planned = vec![
        PlannedPublicBalanceCall {
            public_account_uuid: account.public_account_uuid.clone(),
            asset: PublicBalanceAsset {
                id: PublicAssetId::Native,
                symbol: "ETH".to_string(),
                decimals: 18,
            },
            read: RpcRead::get_balance(account.address),
        },
        PlannedPublicBalanceCall {
            public_account_uuid: account.public_account_uuid.clone(),
            asset: PublicBalanceAsset {
                id: PublicAssetId::Erc20(address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")),
                symbol: "WETH".to_string(),
                decimals: 18,
            },
            read: RpcRead::eth_call(
                address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
                Bytes::new(),
            ),
        },
    ];

    let mut successful_call = planned[0].clone();
    successful_call.public_account_uuid = successful_account.public_account_uuid.clone();
    planned.push(successful_call);
    let observed_at = Instant::now();
    let observed_block = Some(10);
    let snapshot = public_balance_snapshot_from_results(
        1,
        &[account, successful_account],
        &planned,
        vec![Some(U256::from(7_u64)), None, Some(U256::from(9_u64))],
        observed_at,
        observed_block,
    );

    assert_eq!(snapshot.accounts[0].observed_at, None);
    assert_eq!(snapshot.accounts[0].observed_block, None);
    assert_eq!(snapshot.accounts[1].observed_at, Some(observed_at));
    assert_eq!(snapshot.accounts[1].observed_block, observed_block);
    let balances = &snapshot.accounts[0].balances;
    assert_eq!(balances[0].amount.amount(), Some(U256::from(7_u64)));
    assert!(matches!(
        balances[1].amount,
        PublicBalanceAmount::Unavailable
    ));

    let truncated = public_balance_snapshot_from_results(
        1,
        &[snapshot.accounts[0].account.clone()],
        &planned[..2],
        vec![Some(U256::from(7_u64))],
        observed_at,
        observed_block,
    );
    assert_eq!(truncated.accounts[0].observed_at, None);
    assert_eq!(truncated.accounts[0].observed_block, None);
}

#[test]
fn refresh_coordinator_prevents_overlap_and_releases() {
    let coordinator = PublicBalanceRefreshCoordinator::new();
    let guard = coordinator.try_begin().expect("first refresh guard");

    assert!(coordinator.is_refreshing());
    assert!(coordinator.try_begin().is_none());
    drop(guard);
    assert!(!coordinator.is_refreshing());
    assert!(coordinator.try_begin().is_some());
}
