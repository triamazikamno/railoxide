use super::*;

#[test]
fn indexed_artifact_official_source_ignores_stored_custom_fields() {
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Official;
    settings.indexed_artifacts.publisher_pubkey = Some("not-hex".to_string());
    settings.indexed_artifacts.manifest_source = Some(IndexedArtifactManifestSourceSetting::Url(
        "ftp://artifacts.example/manifest.json".to_string(),
    ));
    settings.indexed_artifacts.gateway_urls = vec!["ftp://gateway.example".to_string()];
    settings.indexed_artifacts.max_manifest_age_secs = Some(0);
    settings.indexed_artifacts.concurrency = Some(0);
    settings.indexed_artifacts.max_in_flight_bytes = Some(1024 * 1024 * 1024 + 1);

    settings
        .validate()
        .expect("official mode ignores stored custom-only fields");
    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let source = configs
        .get(1)
        .and_then(|config| {
            config
                .railgun
                .as_ref()
                .and_then(|private| private.sync.indexed_artifact_source.as_ref())
        })
        .expect("official indexed artifact source");

    assert_eq!(
        alloy::hex::encode(source.trusted_publisher_pubkey.as_slice()),
        OFFICIAL_INDEXED_ARTIFACT_PUBLISHER_PUBKEY.trim_start_matches("0x")
    );
    assert!(matches!(
        &source.manifest_source,
        sync_service::IndexedArtifactManifestSource::IpnsName(name)
            if name == OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME
    ));
    assert_eq!(
        source
            .gateway_urls
            .iter()
            .map(|url| url.as_str().to_string())
            .collect::<Vec<_>>(),
        OFFICIAL_POI_ARTIFACT_GATEWAYS
            .iter()
            .map(|gateway| format!("{gateway}/"))
            .collect::<Vec<_>>()
    );
    assert_eq!(source.concurrency, DEFAULT_INDEXED_ARTIFACT_CONCURRENCY);
    assert_eq!(
        source.max_in_flight_bytes,
        DEFAULT_INDEXED_ARTIFACT_MAX_IN_FLIGHT_BYTES
    );
    assert_eq!(source.max_manifest_age, None);
}

#[test]
fn indexed_artifact_custom_source_builds_effective_config() {
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
    settings.indexed_artifacts.publisher_pubkey = Some(format!("0x{}", "11".repeat(32)));
    settings.indexed_artifacts.manifest_source = Some(IndexedArtifactManifestSourceSetting::Url(
        "https://artifacts.example/manifest.json".to_string(),
    ));
    settings.indexed_artifacts.gateway_urls = vec!["https://legacy-gateway.example".to_string()];
    settings.poi.artifact.gateway_urls = vec!["https://canonical-gateway.example".to_string()];
    settings.indexed_artifacts.concurrency = Some(5);
    settings.indexed_artifacts.max_in_flight_bytes = Some(8 * 1024 * 1024);
    settings.indexed_artifacts.max_manifest_age_secs = Some(3_600);

    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let ethereum = configs.get(1).expect("ethereum config");
    let source = ethereum
        .require_railgun()
        .unwrap()
        .sync
        .indexed_artifact_source
        .as_ref()
        .expect("indexed artifact source");

    assert_eq!(
        ethereum
            .require_railgun()
            .unwrap()
            .indexed_artifact_source_mode,
        IndexedArtifactSourceModeSetting::Custom
    );
    assert_eq!(
        alloy::hex::encode(source.trusted_publisher_pubkey.as_slice()),
        "11".repeat(32)
    );
    assert!(matches!(
        &source.manifest_source,
        sync_service::IndexedArtifactManifestSource::Url(url)
            if url.as_str() == "https://artifacts.example/manifest.json"
    ));
    assert_eq!(
        source.gateway_urls[0].as_str(),
        "https://canonical-gateway.example/"
    );
    assert_eq!(source.concurrency, 5);
    assert_eq!(source.max_in_flight_bytes, 8 * 1024 * 1024);
    assert_eq!(
        source.max_manifest_age,
        Some(std::time::Duration::from_hours(1))
    );
}

#[test]
fn indexed_artifact_ipns_source_is_trimmed_in_effective_config() {
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
    settings.indexed_artifacts.publisher_pubkey = Some(format!("0x{}", "22".repeat(32)));
    settings.indexed_artifacts.manifest_source =
        Some(IndexedArtifactManifestSourceSetting::IpnsName(format!(
            "  {OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME}  "
        )));
    settings.indexed_artifacts.gateway_urls = vec!["ftp://legacy-gateway.example".to_string()];

    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let source = configs
        .get(1)
        .and_then(|config| {
            config
                .railgun
                .as_ref()
                .and_then(|private| private.sync.indexed_artifact_source.as_ref())
        })
        .expect("indexed artifact source");

    assert!(matches!(
        &source.manifest_source,
        sync_service::IndexedArtifactManifestSource::IpnsName(name)
            if name == OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME
    ));
}

#[test]
fn indexed_artifact_defaults_apply_to_custom_source_limits() {
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
    settings.indexed_artifacts.publisher_pubkey = Some(format!("0x{}", "22".repeat(32)));
    settings.indexed_artifacts.manifest_source = Some(
        IndexedArtifactManifestSourceSetting::IpnsName("k51qzi5uqu5custom".to_string()),
    );
    settings.indexed_artifacts.gateway_urls = vec!["https://gateway.example".to_string()];

    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let source = configs
        .get(1)
        .and_then(|config| {
            config
                .railgun
                .as_ref()
                .and_then(|private| private.sync.indexed_artifact_source.as_ref())
        })
        .expect("indexed artifact source");

    assert_eq!(source.concurrency, DEFAULT_INDEXED_ARTIFACT_CONCURRENCY);
    assert_eq!(
        source.max_in_flight_bytes,
        DEFAULT_INDEXED_ARTIFACT_MAX_IN_FLIGHT_BYTES
    );
}

#[test]
fn indexed_artifact_custom_source_validation_rejects_missing_source() {
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
    settings.indexed_artifacts.publisher_pubkey = Some("not-hex".to_string());
    settings.indexed_artifacts.manifest_source = Some(IndexedArtifactManifestSourceSetting::Url(
        "ftp://artifacts.example/manifest.json".to_string(),
    ));
    settings.indexed_artifacts.gateway_urls.clear();
    settings
        .indexed_artifacts
        .gateway_urls
        .push("ftp://gateway.example".to_string());
    settings.indexed_artifacts.concurrency = Some(0);
    settings.indexed_artifacts.max_in_flight_bytes = Some(1024 * 1024 * 1024 + 1);

    let err = settings
        .validate()
        .expect_err("bad indexed source rejected");

    assert!(err.messages.iter().any(|message| {
        message.contains("indexed_artifacts.publisher_pubkey") && message.contains("32-byte hex")
    }));
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("indexed_artifacts.manifest_source"))
    );
    assert!(
        !err.messages
            .iter()
            .any(|message| message.contains("indexed_artifacts.gateway_urls"))
    );
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("indexed_artifacts.concurrency"))
    );
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("indexed_artifacts.max_in_flight_bytes"))
    );
}

#[test]
fn default_poi_read_source_converts_to_official_indexed_artifacts() {
    let settings = WalletSettings::default();
    let crate::settings::PoiReadSource::IndexedArtifacts {
        artifact_source: source,
        wallet_read_fallback,
        ..
    } = settings.poi_read_source().expect("POI source")
    else {
        panic!("default POI source should be indexed artifacts");
    };

    assert_eq!(
        alloy::hex::encode(source.trusted_publisher_pubkey.as_slice()),
        OFFICIAL_POI_ARTIFACT_PUBLISHER_PUBKEY.trim_start_matches("0x")
    );
    assert_eq!(
        source.manifest_source,
        crate::settings::PoiArtifactManifestSource::IpnsName(
            OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string()
        )
    );
    assert_eq!(
        source.gateway_urls.len(),
        OFFICIAL_POI_ARTIFACT_GATEWAYS.len()
    );
    assert_eq!(
        wallet_read_fallback,
        crate::settings::PoiProxyFallback::Disabled
    );
    assert_ne!(
        OFFICIAL_POI_ARTIFACT_IPNS_NAME,
        LEGACY_OFFICIAL_POI_ARTIFACT_IPNS_NAME
    );
    assert_ne!(
        OFFICIAL_POI_ARTIFACT_IPNS_NAME,
        OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME
    );
}

#[test]
fn custom_ppoi_artifact_source_converts_without_official_substitution() {
    let mut settings = WalletSettings::default();
    let custom_publisher = format!("0x{}", "42".repeat(32));
    let custom_ipns = "k51qzi5uqu5dicmabkge4lkunc4bkd198u9xicp5espmw5zdzbafkez7hyh5ft".to_string();
    settings
        .poi
        .artifact
        .publisher_pubkey
        .clone_from(&custom_publisher);
    settings.poi.artifact.manifest_source =
        PoiArtifactManifestSourceSetting::IpnsName(custom_ipns.clone());

    let crate::settings::PoiReadSource::IndexedArtifacts {
        artifact_source: source,
        wallet_read_fallback,
        ..
    } = settings.poi_read_source().expect("custom POI source")
    else {
        panic!("custom POI source should use indexed artifacts");
    };

    assert_eq!(
        alloy::hex::encode_prefixed(source.trusted_publisher_pubkey.as_slice()),
        custom_publisher
    );
    assert_eq!(
        source.manifest_source,
        crate::settings::PoiArtifactManifestSource::IpnsName(custom_ipns)
    );
    assert_eq!(
        wallet_read_fallback,
        crate::settings::PoiProxyFallback::Disabled
    );
}

#[test]
fn invalid_or_non_ipns_ppoi_artifact_sources_are_rejected_in_settings() {
    for source in [
        PoiArtifactManifestSourceSetting::IpnsName("not-an-ipns-name".to_string()),
        PoiArtifactManifestSourceSetting::Cid(
            "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku".to_string(),
        ),
        PoiArtifactManifestSourceSetting::Url("https://example.invalid/manifest".to_string()),
    ] {
        let mut settings = WalletSettings::default();
        settings.poi.artifact.manifest_source = source;

        let error = settings
            .poi_read_source()
            .expect_err("non-IPNS PPOI source must fail settings validation");
        assert!(
            error
                .messages
                .iter()
                .any(|message| message.contains("manifest_source"))
        );
    }
}

#[test]
fn explicit_poi_proxy_mode_remains_selectable_without_artifact_fallback() {
    let mut settings = legacy_official_settings();
    settings.poi.read_source = PoiReadSourceSetting::PoiProxy;
    settings.poi.proxy.rpc_url = "https://explicit-poi-proxy.example/rpc".to_string();

    let crate::settings::PoiReadSource::PoiProxy { rpc_url } =
        settings.poi_read_source().expect("explicit proxy source")
    else {
        panic!("explicit proxy mode should remain selected");
    };
    assert_eq!(
        rpc_url,
        crate::settings::SensitiveUrl::from(
            reqwest::Url::parse("https://explicit-poi-proxy.example/rpc")
                .expect("explicit proxy URL")
        )
    );
}

#[test]
fn poi_proxy_mode_still_validates_canonical_artifact_gateways() {
    let mut settings = WalletSettings::default();
    settings.poi.read_source = PoiReadSourceSetting::PoiProxy;
    settings.poi.artifact.gateway_urls = vec!["ftp://invalid-gateway.example".to_string()];

    let err = settings
        .validate()
        .expect_err("invalid canonical gateway should be rejected");
    assert_eq!(
        err.messages
            .iter()
            .filter(|message| message.contains("poi.artifact.gateway_urls"))
            .count(),
        1
    );
}

#[test]
fn default_poi_rpc_url_matches_default_poi_service() {
    let settings = WalletSettings::default();

    assert_eq!(
        settings.poi_rpc_url().expect("POI RPC URL"),
        crate::settings::SensitiveUrl::from(
            reqwest::Url::parse(poi::poi::DEFAULT_WALLET_POI_RPC_URL).expect("default POI RPC URL")
        )
    );
}

#[test]
fn custom_poi_rpc_url_is_runtime_url() {
    let mut settings = WalletSettings::default();
    settings.poi.proxy.rpc_url = "https://poi.example/rpc".to_string();

    assert_eq!(
        settings.poi_rpc_url().expect("custom POI RPC URL"),
        crate::settings::SensitiveUrl::from(
            reqwest::Url::parse("https://poi.example/rpc").expect("custom POI RPC URL")
        )
    );
}

#[test]
fn poi_runtime_policy_formatting_redacts_configured_endpoints() {
    let mut settings = WalletSettings::default();
    settings.poi.proxy.rpc_url =
        "https://rpc-user-sentinel:rpc-password-sentinel@rpc-host-sentinel.invalid/rpc-path-sentinel?rpc-query-sentinel#rpc-fragment-sentinel".to_string();
    settings.poi.artifact.manifest_source = crate::settings::PoiArtifactManifestSourceSetting::Url(
        "https://manifest-user-sentinel:manifest-password-sentinel@manifest-host-sentinel.invalid/manifest-path-sentinel?manifest-query-sentinel#manifest-fragment-sentinel".to_string(),
    );
    settings.poi.artifact.gateway_urls = vec![
        "https://gateway-user-sentinel:gateway-password-sentinel@gateway-host-sentinel.invalid/gateway-path-sentinel?gateway-query-sentinel#gateway-fragment-sentinel".to_string(),
    ];

    let policy = crate::settings::PoiReadSource::IndexedArtifacts {
        artifact_source: settings.poi.artifact.source_config(),
        rpc_url: settings.poi_rpc_url().expect("sensitive POI RPC URL"),
        wallet_read_fallback: crate::settings::PoiProxyFallback::Disabled,
    };
    let formatted = format!("{policy:?}");
    for sentinel in [
        "rpc-user-sentinel",
        "rpc-password-sentinel",
        "rpc-host-sentinel",
        "rpc-path-sentinel",
        "rpc-query-sentinel",
        "rpc-fragment-sentinel",
        "manifest-user-sentinel",
        "manifest-password-sentinel",
        "manifest-host-sentinel",
        "manifest-path-sentinel",
        "manifest-query-sentinel",
        "manifest-fragment-sentinel",
        "gateway-user-sentinel",
        "gateway-password-sentinel",
        "gateway-host-sentinel",
        "gateway-path-sentinel",
        "gateway-query-sentinel",
        "gateway-fragment-sentinel",
    ] {
        assert!(
            !formatted.contains(sentinel),
            "leaked {sentinel}: {formatted}"
        );
    }
    assert!(settings.poi.proxy.rpc_url.contains("rpc-user-sentinel"));
    assert!(matches!(
        settings.poi.artifact.manifest_source,
        crate::settings::PoiArtifactManifestSourceSetting::Url(ref url)
            if url.contains("manifest-user-sentinel")
    ));
    assert!(settings.poi.artifact.gateway_urls[0].contains("gateway-user-sentinel"));
}
