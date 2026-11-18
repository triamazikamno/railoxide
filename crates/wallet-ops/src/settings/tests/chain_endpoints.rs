use super::*;

#[test]
fn effective_chain_configs_apply_supported_overrides_in_order() {
    let mut settings = WalletSettings::default();
    let ethereum = settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings");
    ethereum.rpc_endpoints = vec![
        "https://rpc-a.example".to_string().into(),
        "https://rpc-b.example".to_string().into(),
    ];
    ethereum.railgun.quick_sync.endpoint = Some("https://quick.example/graphql".to_string());
    ethereum.finality_depth = Some(64);
    ethereum.contracts.multicall_contract =
        Some("0x0000000000000000000000000000000000000001".to_string());
    ethereum.railgun.deployment.archive_until_block = Some(44);
    ethereum.railgun.deployment.archive_rpc_url = Some("https://archive.example".to_string());
    ethereum.gas.gas_limit_buffer = Some(250_000);

    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let ethereum = configs.get(1).expect("ethereum config");

    assert_eq!(
        ethereum.rpc_route.endpoint_urls(),
        vec![
            Url::parse("https://rpc-a.example").unwrap(),
            Url::parse("https://rpc-b.example").unwrap(),
        ]
    );
    assert_eq!(
        ethereum
            .require_railgun()
            .unwrap()
            .sync
            .quick_sync_endpoint
            .as_ref()
            .map(Url::as_str),
        Some("https://quick.example/graphql")
    );
    assert_eq!(ethereum.finality_depth, 64);
    assert_eq!(
        ethereum.require_railgun().unwrap().sync.archive_until_block,
        44
    );
    assert_eq!(
        ethereum
            .require_railgun()
            .unwrap()
            .archive_rpc_url
            .as_ref()
            .map(|url| url.expose_url().as_str()),
        Some("https://archive.example/")
    );
    assert_eq!(
        ethereum.rpc_route.multicall(),
        Some(Address::from_str("0x0000000000000000000000000000000000000001").unwrap())
    );
    assert_eq!(ethereum.gas.gas_limit_buffer, 250_000);
}

#[test]
fn effective_chain_configs_carry_endpoint_transports_and_roles() {
    let mut settings = WalletSettings::default();
    let ethereum = settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings");
    let presets = crate::settings::presets::EvmPreset::for_chain(1)
        .unwrap()
        .rpc_endpoints;
    ethereum.rpc_endpoints = presets
        .iter()
        .cloned()
        .map(crate::settings::RpcEndpointSetting::from)
        .collect();
    ethereum
        .rpc_endpoints
        .push(crate::settings::RpcEndpointSetting {
            url: "http://127.0.0.1:8545".to_string(),
            direct: true,
            general: false,
            submit: false,
            ..crate::settings::RpcEndpointSetting::default()
        });
    ethereum.railgun.deployment.archive_rpc_url = Some("http://127.0.0.1:8546".to_string());
    ethereum.railgun.deployment.archive_rpc_direct = true;

    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let ethereum = configs.get(1).expect("ethereum config");
    let node = Url::parse("http://127.0.0.1:8545").unwrap();
    let mut expected = presets
        .iter()
        .map(|url| (Url::parse(url).unwrap(), RpcTransport::Wallet))
        .collect::<Vec<_>>();
    expected.push((node.clone(), RpcTransport::Direct));
    assert_eq!(
        ethereum
            .rpc_route
            .transport_endpoints()
            .map(|(endpoint, transport)| (endpoint.expose_url().clone(), transport))
            .collect::<Vec<_>>(),
        expected
    );
    let class_urls = |class| {
        ethereum
            .rpc_route
            .for_class(class)
            .expect("class has endpoints")
            .endpoint_urls()
    };
    assert!(class_urls(crate::RpcTrafficClass::Sensitive).contains(&node));
    for class in [
        crate::RpcTrafficClass::General,
        crate::RpcTrafficClass::Submission,
    ] {
        assert!(!class_urls(class).contains(&node));
    }
    assert!(ethereum.rpc_route.requires_identity_verification());
    assert_eq!(
        ethereum.require_railgun().unwrap().archive_rpc_transport,
        RpcTransport::Direct
    );
    assert_eq!(configs.direct_rpc_endpoint_count(), 2);
}

#[test]
fn endpoint_lists_must_cover_every_traffic_class() {
    let endpoint = |url: &str, general, sensitive, submit| crate::settings::RpcEndpointSetting {
        url: url.to_string(),
        direct: false,
        general,
        sensitive,
        submit,
    };
    let with_endpoints = |endpoints: Vec<crate::settings::RpcEndpointSetting>| {
        let mut settings = WalletSettings::default();
        settings
            .chains
            .per_chain
            .get_mut(&1)
            .expect("ethereum settings")
            .rpc_endpoints = endpoints.clone();
        let mut custom = custom_evm_chain();
        custom.rpc_endpoints = endpoints;
        settings.chains.custom.insert(999_999, custom);
        settings
    };

    for (class, [general, sensitive, submit]) in [
        ("General", [false, true, true]),
        ("Sensitive", [true, false, true]),
        ("Submit", [true, true, false]),
    ] {
        let error = with_endpoints(vec![
            endpoint("https://a.example", general, sensitive, submit),
            endpoint("https://b.example", general, sensitive, submit),
        ])
        .validate()
        .expect_err(class);
        for chain in ["chains.per_chain.1", "chains.custom.999999"] {
            let expected = format!("{chain}.rpc_endpoints must include a {class} endpoint");
            assert_eq!(
                error
                    .messages
                    .iter()
                    .filter(|message| message.starts_with(&format!("{chain}.")))
                    .collect::<Vec<_>>(),
                [&expected]
            );
        }
    }

    // Classes may come from different endpoints.
    with_endpoints(vec![
        endpoint("https://a.example", true, false, false),
        endpoint("https://b.example", false, true, false),
        endpoint("https://c.example", false, false, true),
    ])
    .validate()
    .expect("classes spread across endpoints");
}

#[test]
fn tron_defaults_are_the_presets_in_order() {
    let settings = WalletSettings::default();
    settings.validate().expect("defaults are valid");
    let endpoints = settings.tron.effective_endpoints();
    assert_eq!(
        endpoints
            .iter()
            .map(|endpoint| (
                endpoint.json_rpc_url.as_str(),
                endpoint.http_api_url.as_deref()
            ))
            .collect::<Vec<_>>(),
        [
            (
                "https://tron.api.pocket.network",
                Some("https://tron.api.pocket.network")
            ),
            (
                "https://api.trongrid.io/jsonrpc",
                Some("https://api.trongrid.io")
            ),
            (
                "https://tron-rpc.publicnode.com/jsonrpc",
                Some("https://tron-rpc.publicnode.com")
            ),
        ]
    );
}

#[test]
fn tron_endpoint_validation_rejects_each_rule() {
    let endpoint = |url: &str, http: bool, [general, sensitive, submit]: [bool; 3]| {
        crate::settings::TronEndpointSetting {
            json_rpc_url: url.to_owned(),
            http_api_url: http.then(|| "https://api.example".to_owned()),
            direct: false,
            general,
            sensitive,
            submit,
        }
    };
    let ok = |url: &str| endpoint(url, true, [true; 3]);
    let long_url = format!("https://a.example/{}", "a".repeat(4096));
    let mut ws_api = ok("https://a.example");
    ws_api.http_api_url = Some("ws://api.example".into());

    for (endpoints, expected) in [
        (
            vec![endpoint("https://a.example", true, [false, true, true])],
            vec!["tron.endpoints must include a General endpoint".to_owned()],
        ),
        (
            vec![endpoint("https://a.example", true, [true, false, true])],
            vec![
                "tron.endpoints must include a Sensitive endpoint".to_owned(),
                "tron.endpoints: sending needs a Sensitive endpoint with the HTTP API".to_owned(),
            ],
        ),
        (
            vec![endpoint("https://a.example", true, [true, true, false])],
            vec!["tron.endpoints must include a Submit endpoint".to_owned()],
        ),
        (
            vec![
                ok("https://a.example"),
                endpoint("https://b.example", false, [false, false, true]),
            ],
            vec![
                "tron.endpoints[1] accepts Submit without an HTTP API URL; broadcasting needs the HTTP API"
                    .to_owned(),
            ],
        ),
        (
            vec![
                endpoint("https://a.example", false, [true, true, false]),
                endpoint("https://b.example", true, [false, false, true]),
            ],
            vec![
                "tron.endpoints: sending needs a Sensitive endpoint with the HTTP API".to_owned(),
            ],
        ),
        (
            vec![ok("ftp://a.example")],
            vec![
                "tron.endpoints[0].json_rpc_url must use one of these URL schemes: http, https; got ftp"
                    .to_owned(),
            ],
        ),
        (
            vec![ws_api],
            vec![
                "tron.endpoints[0].http_api_url must use one of these URL schemes: http, https; got ws"
                    .to_owned(),
            ],
        ),
        (
            vec![ok(long_url.as_str())],
            vec!["tron.endpoints[0].json_rpc_url is too long".to_owned()],
        ),
        (
            vec![
                endpoint("https://a.example", true, [true, false, false]),
                endpoint("https://A.example/", true, [false, true, true]),
            ],
            vec!["tron.endpoints[1].json_rpc_url duplicates tron.endpoints[0]".to_owned()],
        ),
        (
            (0..=crate::settings::MAX_CHAIN_ENDPOINTS)
                .map(|index| ok(&format!("https://{index}.example")))
                .collect(),
            vec!["tron.endpoints has too many RPC endpoints".to_owned()],
        ),
    ] {
        let mut settings = WalletSettings::default();
        settings.tron.endpoints = Some(endpoints);
        let error = settings.validate().expect_err(&expected[0]);
        assert_eq!(error.messages, expected);
    }
}

#[test]
fn effective_chain_configs_apply_quick_sync_bounds_and_disabled_state() {
    let mut settings = WalletSettings::default();
    let ethereum = settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings");
    ethereum.railgun.quick_sync.enabled = false;
    ethereum.railgun.quick_sync.indexed_wallet_block_range = Some(25_000);
    ethereum.railgun.block_range = Some(2_000);
    ethereum.railgun.poll_interval_secs = Some(30);

    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let ethereum = configs.get(1).expect("ethereum config");

    assert!(
        ethereum
            .require_railgun()
            .unwrap()
            .sync
            .quick_sync_endpoint
            .is_none()
    );
    assert_eq!(
        ethereum
            .require_railgun()
            .unwrap()
            .sync
            .indexed_wallet_block_range,
        25_000
    );
    assert_eq!(ethereum.require_railgun().unwrap().sync.block_range, 2_000);
    assert_eq!(
        ethereum.require_railgun().unwrap().sync.poll_interval,
        std::time::Duration::from_secs(30)
    );
}

#[test]
fn chain_reset_restores_supported_chain_defaults() {
    let mut settings = WalletSettings::default();
    settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings")
        .enabled = false;

    let custom = custom_evm_chain();
    settings.chains.custom.insert(999_999, custom.clone());
    settings.reset_chains();

    assert_eq!(
        settings.chains.per_chain,
        crate::settings::ChainSettings::default().per_chain
    );
    assert_eq!(settings.chains.custom.get(&999_999), Some(&custom));
    assert!(settings.chains.enabled_chain_ids().contains(&1));
}

#[test]
fn effective_chain_configs_reject_unsupported_chain_ids() {
    let mut settings = WalletSettings::default();
    settings
        .chains
        .per_chain
        .insert(31337, crate::settings::ChainSettingsOverride::default());

    let err = build_effective_chain_configs(&settings).expect_err("unsupported chain rejected");
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("provide a complete custom definition"))
    );
}
