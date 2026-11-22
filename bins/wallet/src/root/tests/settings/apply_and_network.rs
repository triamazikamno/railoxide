use super::*;

#[test]
fn settings_apply_classifier_tracks_restart_and_request_changes() {
    let saved = WalletSettings::default();
    assert_eq!(
        classify_settings_apply_mode(&saved, &saved),
        SettingsApplyMode::Clean
    );

    // Each build creates its own manifest reuse handle, which is not a
    // configuration change.
    let mut official = saved.clone();
    official.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Official;
    let first_build = build_effective_chain_configs(&official).unwrap();
    let second_build = build_effective_chain_configs(&official).unwrap();
    let (first, second) = (first_build.get(1).unwrap(), second_build.get(1).unwrap());
    assert!(
        first
            .railgun
            .as_ref()
            .is_some_and(|railgun| railgun.sync.indexed_artifact_source.is_some())
    );
    assert_eq!(first, second);
    assert!(first.operationally_matches(second));

    let mut pricing = saved.clone();
    pricing
        .chains
        .per_chain
        .get_mut(&1)
        .unwrap()
        .native_usd_pricing = wallet_ops::settings::NativeUsdPricing::Disabled;
    assert_eq!(
        classify_settings_apply_mode(&saved, &pricing),
        SettingsApplyMode::NewRequests
    );
    let old_chain = build_effective_chain_configs(&saved).unwrap();
    let new_chain = build_effective_chain_configs(&pricing).unwrap();
    assert!(
        old_chain
            .get(1)
            .unwrap()
            .operationally_matches(new_chain.get(1).unwrap())
    );
    pricing.chains.per_chain.get_mut(&1).unwrap().rpc_endpoints =
        vec!["https://rpc.example".to_owned().into()];
    assert_eq!(
        classify_settings_apply_mode(&saved, &pricing),
        SettingsApplyMode::NetworkingRestart
    );

    let mut network_draft = saved.clone();
    network_draft.network.mode = NetworkModeSetting::Direct;
    assert_eq!(
        classify_settings_apply_mode(&saved, &network_draft),
        SettingsApplyMode::NetworkingRestart
    );

    let mut indexed_draft = saved.clone();
    indexed_draft.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
    assert_eq!(
        classify_settings_apply_mode(&saved, &indexed_draft),
        SettingsApplyMode::NetworkingRestart
    );

    let mut request_draft = saved.clone();
    request_draft.broadcaster.response_timeout_secs += 1;
    assert_eq!(
        classify_settings_apply_mode(&saved, &request_draft),
        SettingsApplyMode::NewRequests
    );

    let mut privacy_draft = saved.clone();
    privacy_draft.privacy.mimic_railway_shields_by_default = true;
    assert_eq!(
        classify_settings_apply_mode(&saved, &privacy_draft),
        SettingsApplyMode::NewRequests
    );

    let mut walletconnect_draft = saved.clone();
    walletconnect_draft.walletconnect.project_id_override = Some("project-override".to_owned());
    assert_eq!(
        classify_settings_apply_mode(&saved, &walletconnect_draft),
        SettingsApplyMode::NewRequests
    );

    let mut session_draft = saved.clone();
    session_draft.runtime.public_balance_refresh_interval_secs += 1;
    assert_eq!(
        classify_settings_apply_mode(&saved, &session_draft),
        SettingsApplyMode::FutureSessions
    );

    let mut auto_lock_draft = saved.clone();
    auto_lock_draft.runtime.auto_lock_timeout_secs = None;
    assert_eq!(
        classify_settings_apply_mode(&saved, &auto_lock_draft),
        SettingsApplyMode::FutureSessions
    );
}

#[test]
fn auto_lock_settings_helpers_cover_presets_disabled_discard_and_defaults() {
    let options = auto_lock_timeout_options();
    assert_eq!(options.first().expect("Disabled option").1, "Disabled");
    assert_eq!(
        options
            .iter()
            .find(|(value, _label)| value.as_ref() == "900")
            .expect("15-minute option")
            .1,
        "15 minutes"
    );
    for (value, _label) in options {
        let policy = auto_lock_timeout_from_value(value.as_ref());
        assert_eq!(auto_lock_timeout_value(policy), value);
    }

    let defaults = WalletSettings::default();
    assert_eq!(defaults.runtime.auto_lock_timeout_secs, Some(15 * 60));
    let mut saved = defaults;
    saved.runtime.auto_lock_timeout_secs = None;
    let mut draft = saved.clone();
    draft.runtime.auto_lock_timeout_secs = Some(30 * 60);
    assert_eq!(
        settings_draft_after_discard(&saved)
            .runtime
            .auto_lock_timeout_secs,
        None
    );
    assert_eq!(
        WalletSettings::reset_to_defaults()
            .runtime
            .auto_lock_timeout_secs,
        Some(15 * 60)
    );
}

#[test]
fn settings_save_action_requires_restart_for_networking_changes() {
    let saved = WalletSettings::default();
    let mut network_draft = saved.clone();
    network_draft.network.mode = NetworkModeSetting::Direct;
    assert!(!settings_save_action_enabled(&saved, &network_draft, false));
    assert!(settings_restart_action_enabled(
        &saved,
        &network_draft,
        false,
        true,
    ));

    let mut request_draft = saved.clone();
    request_draft.broadcaster.response_timeout_secs += 1;
    assert!(settings_save_action_enabled(&saved, &request_draft, false));
    assert!(settings_restart_action_enabled(
        &saved,
        &request_draft,
        false,
        true,
    ));

    assert!(!settings_save_action_enabled(&saved, &request_draft, true));
    assert!(!settings_restart_action_enabled(
        &saved,
        &request_draft,
        true,
        true,
    ));
    assert!(!settings_restart_action_enabled(
        &saved,
        &request_draft,
        false,
        false,
    ));
}

#[test]
fn anchor_bps_formatting_shows_percent_and_exact_bps() {
    assert_eq!(format_anchor_bps_percent(9_000), "90%");
    assert_eq!(format_anchor_bps_percent(9_050), "90.5%");
    assert_eq!(format_anchor_bps_percent(9_055), "90.55%");
    assert_eq!(
        format_anchor_bps_percent_range(9_000, 15_000),
        "90% - 150% of price anchor"
    );
    assert_eq!(
        format_anchor_premium_range(9_000, 15_000),
        "Allows -10% to +50% vs anchor"
    );
    assert_eq!(
        format_anchor_bps_exact_range(9_000, 15_000),
        "9,000 - 15,000 bps"
    );
}

#[test]
fn settings_restart_reuses_network_only_when_network_settings_are_unchanged() {
    let saved = WalletSettings::default();

    let mut waku_draft = saved.clone();
    waku_draft.waku.max_peers += 1;
    assert!(settings_restart_reuses_active_network(&saved, &waku_draft));

    let mut poi_draft = saved.clone();
    poi_draft.poi.read_source = PoiReadSourceSetting::PoiProxy;
    assert!(settings_restart_reuses_active_network(&saved, &poi_draft));

    let mut indexed_draft = saved.clone();
    indexed_draft.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
    assert!(settings_restart_reuses_active_network(
        &saved,
        &indexed_draft
    ));

    let mut network_draft = saved.clone();
    network_draft.network.mode = NetworkModeSetting::Direct;
    assert!(!settings_restart_reuses_active_network(
        &saved,
        &network_draft
    ));
}

#[test]
fn proxy_url_setting_only_shows_for_proxy_mode() {
    assert!(!should_show_proxy_url_setting(NetworkModeSetting::Tor));
    assert!(should_show_proxy_url_setting(NetworkModeSetting::Proxy));
    assert!(!should_show_proxy_url_setting(NetworkModeSetting::Direct));
}

#[test]
fn only_socks5_proxy_url_warns_about_local_dns() {
    let mut settings = WalletSettings::default();
    settings.network.mode = NetworkModeSetting::Proxy;
    for (url, warns) in [
        ("socks5://127.0.0.1:9050", true),
        ("SOCKS5://127.0.0.1:9050", true),
        ("socks5h://127.0.0.1:9050", false),
        ("http://127.0.0.1:8080", false),
    ] {
        settings.network.proxy_url = Some(url.to_string());
        assert_eq!(proxy_url_resolves_dns_locally(&settings), warns, "{url}");
    }
}

#[test]
fn proxy_waku_disclaimer_only_shows_for_proxy_mode() {
    assert!(!should_show_proxy_waku_disclaimer(NetworkModeSetting::Tor));
    assert!(should_show_proxy_waku_disclaimer(NetworkModeSetting::Proxy));
    assert!(!should_show_proxy_waku_disclaimer(
        NetworkModeSetting::Direct
    ));
}

#[test]
fn waku_doh_settings_display_presets_until_customized() {
    let settings = WalletSettings::default();

    assert_eq!(
        display_waku_doh_endpoint(&settings),
        DEFAULT_TOR_DOH_ENDPOINT
    );
    assert_eq!(
        display_waku_doh_fallback_endpoints(&settings),
        vec![DEFAULT_DOH_ENDPOINT.to_string()]
    );
    assert!(settings.waku.doh_endpoint.is_none());
    assert!(settings.waku.doh_fallback_endpoints.is_none());

    let mut direct = settings.clone();
    direct.network.mode = NetworkModeSetting::Direct;
    assert_eq!(display_waku_doh_endpoint(&direct), DEFAULT_DOH_ENDPOINT);
    assert_eq!(
        display_waku_doh_fallback_endpoints(&direct),
        [] as [String; 0]
    );

    let mut proxy = settings.clone();
    proxy.network.mode = NetworkModeSetting::Proxy;
    proxy.network.proxy_url = Some("socks5h://127.0.0.1:9050".to_string());
    assert_eq!(display_waku_doh_endpoint(&proxy), DEFAULT_DOH_ENDPOINT);
    assert_eq!(
        display_waku_doh_fallback_endpoints(&proxy),
        [] as [String; 0]
    );

    let mut custom = settings;
    custom.waku.doh_endpoint = Some("https://doh.example.invalid/dns-query".to_string());
    assert_eq!(
        display_waku_doh_endpoint(&custom),
        "https://doh.example.invalid/dns-query"
    );
    assert_eq!(
        display_waku_doh_fallback_endpoints(&custom),
        vec![DEFAULT_DOH_ENDPOINT.to_string()]
    );
}

#[test]
fn waku_doh_fallback_mutations_materialize_presets() {
    let mut settings = WalletSettings::default();

    remove_waku_doh_fallback_endpoint(&mut settings, 0);
    assert_eq!(settings.waku.doh_fallback_endpoints, Some(Vec::new()));
    assert_eq!(
        display_waku_doh_fallback_endpoints(&settings),
        [] as [String; 0]
    );

    add_waku_doh_fallback_endpoint(&mut settings, " https://fallback.example/dns-query ");
    assert_eq!(
        settings.waku.doh_fallback_endpoints.as_deref(),
        Some(["https://fallback.example/dns-query".to_string()].as_slice())
    );

    set_waku_doh_fallback_endpoint(&mut settings, 0, " https://edited.example/dns-query ");
    assert_eq!(
        settings.waku.doh_fallback_endpoints.as_deref(),
        Some(["https://edited.example/dns-query".to_string()].as_slice())
    );
}

#[test]
fn waku_dns_enr_tree_settings_display_presets_until_customized() {
    let mut settings = WalletSettings::default();

    assert_eq!(
        display_waku_dns_enr_trees(&settings),
        default_waku_dns_enr_trees()
    );
    assert!(settings.waku.dns_enr_trees.is_none());

    remove_waku_dns_enr_tree(&mut settings, 0);
    assert_eq!(settings.waku.dns_enr_trees, Some(Vec::new()));
    assert_eq!(display_waku_dns_enr_trees(&settings), [] as [String; 0]);

    add_waku_dns_enr_tree(&mut settings, " enrtree://custom@example.invalid ");
    assert_eq!(
        settings.waku.dns_enr_trees.as_deref(),
        Some(["enrtree://custom@example.invalid".to_string()].as_slice())
    );

    set_waku_dns_enr_tree(&mut settings, 0, " enrtree://edited@example.invalid ");
    assert_eq!(
        settings.waku.dns_enr_trees.as_deref(),
        Some(["enrtree://edited@example.invalid".to_string()].as_slice())
    );
}

#[test]
fn waku_peer_mutations_keep_lists_separate_and_preserve_opt_out() {
    use crate::root::settings::{add_waku_peer, remove_waku_peer, set_waku_peer};

    let mut settings = WalletSettings::default();
    let backup = wallet_ops::settings::default_waku_backup_peers().remove(0);
    let mut direct = backup.clone();
    direct.addr = "/dns4/direct.example/tcp/8000/wss".into();

    add_waku_peer(&mut settings, WakuPeerList::Direct, direct.clone());
    assert_eq!(WakuPeerList::Backup.peers(&settings), vec![backup.clone()]);

    let mut edited_backup = backup;
    edited_backup.addr = "/dns4/backup.example/tcp/8000/wss".into();
    set_waku_peer(
        &mut settings,
        WakuPeerList::Backup,
        0,
        edited_backup.clone(),
    );
    assert_eq!(WakuPeerList::Backup.peers(&settings), vec![edited_backup]);
    assert_eq!(WakuPeerList::Direct.peers(&settings), vec![direct.clone()]);

    remove_waku_peer(&mut settings, WakuPeerList::Backup, 0);
    assert_eq!(settings.waku.backup_peers, Some(Vec::new()));
    assert_eq!(
        WakuPeerList::Backup.peers(&settings),
        [] as [wallet_ops::settings::WakuDirectPeerSetting; 0]
    );
    assert_eq!(WakuPeerList::Direct.peers(&settings), vec![direct]);
}

#[test]
fn poi_gateway_settings_mutations_update_direct_list() {
    let mut settings = WalletSettings::default();
    settings.poi.artifact.gateway_urls = vec![
        "https://gateway-one.example".to_string(),
        "https://gateway-two.example".to_string(),
    ];

    set_poi_gateway_url(&mut settings, 0, " https://edited-gateway.example ");
    add_poi_gateway_url(&mut settings, " https://added-gateway.example ");
    remove_poi_gateway_url(&mut settings, 1);
    remove_poi_gateway_url(&mut settings, 10);

    assert_eq!(
        settings.poi.artifact.gateway_urls,
        vec![
            "https://edited-gateway.example".to_string(),
            "https://added-gateway.example".to_string(),
        ]
    );
}

#[test]
fn indexed_artifact_source_mode_helpers_apply_official_and_disabled_presets() {
    let mut settings = WalletSettings::default();

    apply_indexed_artifact_source_mode(&mut settings, "official");
    assert_eq!(
        indexed_artifact_source_mode_value(settings.indexed_artifacts.source_mode),
        "official"
    );
    assert_eq!(
        settings.indexed_artifacts,
        IndexedArtifactSettings::official_preset()
    );
    assert!(indexed_artifact_source_status_message(&settings).contains("Official"));

    apply_indexed_artifact_source_mode(&mut settings, "custom");
    assert_eq!(
        settings.indexed_artifacts.source_mode,
        IndexedArtifactSourceModeSetting::Custom
    );

    apply_indexed_artifact_source_mode(&mut settings, "disabled");
    assert_eq!(
        settings.indexed_artifacts,
        IndexedArtifactSettings::disabled_preset()
    );
    assert_eq!(
        indexed_artifact_source_status_message(&settings),
        "Squid quick-sync -> RPC"
    );
}

#[test]
fn indexed_artifact_custom_settings_only_show_for_custom_mode() {
    assert!(!should_show_indexed_artifact_custom_settings(
        IndexedArtifactSourceModeSetting::Official
    ));
    assert!(!should_show_indexed_artifact_custom_settings(
        IndexedArtifactSourceModeSetting::Disabled
    ));
    assert!(should_show_indexed_artifact_custom_settings(
        IndexedArtifactSourceModeSetting::Custom
    ));
}

#[test]
fn amount_statistics_name_is_optional_for_a_custom_source_and_marked_when_invalid() {
    let mut settings = WalletSettings::default();
    apply_indexed_artifact_source_mode(&mut settings, "custom");
    settings.validate().expect("an empty name is valid");
    let empty = indexed_artifact_amount_statistics_help(&settings, None);
    assert!(empty.contains("No amount statistics are fetched for this source"));
    assert!(empty.contains("show only the privacy factors that need none"));
    assert!(
        !empty.contains("off"),
        "the hints stay on without statistics"
    );

    settings.indexed_artifacts.amount_statistics_ipns_name = Some("not-an-ipns-name".to_owned());
    let error = settings.validate().unwrap_err().to_string();
    let invalid = indexed_artifact_amount_statistics_help(&settings, Some(&error));
    assert!(invalid.contains("isn't a valid IPNS name"), "{invalid}");

    settings.indexed_artifacts.amount_statistics_ipns_name =
        Some(wallet_ops::settings::OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME.to_owned());
    settings.validate().expect("a valid name is accepted");
    assert!(
        !indexed_artifact_amount_statistics_help(&settings, None).contains("isn't a valid"),
        "a valid name is not marked"
    );
}
