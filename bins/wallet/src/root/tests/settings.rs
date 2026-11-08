use super::*;
use crate::root::chain_load::ChainSyncProgress;
use crate::root::maintenance::{
    PublicSyncResetCompletion, WalletMaintenanceReset, WalletMaintenanceStateMachine,
    public_sync_reset_restart_is_safe,
};
use crate::root::settings::{
    TRON_ADDRESS_NEEDED, apply_indexed_artifact_source_mode, display_token_address,
    indexed_artifact_source_mode_value, indexed_artifact_source_status_message,
    price_anchor_component_dialog_values_from_anchor, price_anchor_dialog_values_from_override,
    should_show_indexed_artifact_custom_settings, stored_token_address,
};
use crate::root::startup::tor_bootstrap_recovery_is_current;
use wallet_ops::WalletNetworkProgressStage;
use wallet_ops::settings::{IndexedArtifactSettings, IndexedArtifactSourceModeSetting};

#[test]
fn private_tab_is_default_wallet_tab() {
    assert_eq!(WalletTab::default(), WalletTab::Private);
}

#[test]
fn maintenance_reset_state_machine_rejects_competing_acquisition() {
    let mut state = WalletMaintenanceStateMachine::default();
    let public_generation = state
        .try_acquire(WalletMaintenanceReset::Public, "public reset")
        .expect("idle state acquires public reset");

    assert_eq!(state.reset(), WalletMaintenanceReset::Public);
    assert_eq!(state.status().as_deref(), Some("public reset"));
    assert!(
        state
            .try_acquire(WalletMaintenanceReset::Merkle, "merkle reset")
            .is_none()
    );
    assert!(
        state
            .try_acquire(WalletMaintenanceReset::Poi, "PPOI reset")
            .is_none()
    );
    assert!(state.complete(public_generation, "public reset complete"));
    assert_eq!(state.reset(), WalletMaintenanceReset::Idle);
    let poi_generation = state
        .try_acquire(WalletMaintenanceReset::Poi, "PPOI reset")
        .expect("idle state acquires PPOI reset");
    assert_eq!(state.reset(), WalletMaintenanceReset::Poi);
    assert!(
        state
            .try_acquire(WalletMaintenanceReset::Public, "public reset")
            .is_none()
    );
    assert!(state.complete(poi_generation, "PPOI reset complete"));
    assert!(state.set_idle_status("cleanup pending"));
    assert_eq!(state.status().as_deref(), Some("cleanup pending"));
}

#[test]
fn public_and_ppoi_resets_block_sync_for_newly_installed_root() {
    assert!(maintenance_blocks_public_sync(
        WalletMaintenanceReset::Public
    ));
    assert!(maintenance_blocks_public_sync(WalletMaintenanceReset::Poi));
    assert!(!maintenance_blocks_public_sync(
        WalletMaintenanceReset::Merkle
    ));
    assert!(!maintenance_blocks_public_sync(
        WalletMaintenanceReset::Idle
    ));
}

#[test]
fn public_sync_reset_restarts_only_after_cleanup_reaches_reset_attempt() {
    assert!(public_sync_reset_restart_is_safe(
        true,
        PublicSyncResetCompletion::ResetAttempted,
    ));
    assert!(!public_sync_reset_restart_is_safe(
        true,
        PublicSyncResetCompletion::CleanupFailed,
    ));
    assert!(!public_sync_reset_restart_is_safe(
        false,
        PublicSyncResetCompletion::ResetAttempted,
    ));
}

#[test]
fn maintenance_reset_state_machine_ignores_stale_completion() {
    let mut state = WalletMaintenanceStateMachine::default();
    let generation = state
        .try_acquire(WalletMaintenanceReset::Merkle, "merkle reset")
        .expect("idle state acquires merkle reset");

    assert!(!state.complete(generation.wrapping_add(1), "stale completion"));
    assert_eq!(state.reset(), WalletMaintenanceReset::Merkle);
    assert_eq!(state.status().as_deref(), Some("merkle reset"));
    assert!(state.complete(generation, "merkle reset complete"));
    assert_eq!(state.reset(), WalletMaintenanceReset::Idle);
    assert_eq!(state.status().as_deref(), Some("merkle reset complete"));
}

#[test]
fn utxo_table_focus_is_activity_scoped() {
    let state = ChainUtxoState::Loading {
        progress: ChainSyncProgress::default(),
    };

    assert!(!should_focus_utxo_table(
        Activity::Wallet,
        WalletTab::Private,
        Some(&state)
    ));
    assert!(!should_focus_utxo_table(
        Activity::Wallet,
        WalletTab::Activity,
        Some(&state)
    ));
    assert!(!should_focus_utxo_table(
        Activity::Broadcaster,
        WalletTab::UTXOs,
        Some(&state)
    ));
    assert!(should_focus_utxo_table(
        Activity::Wallet,
        WalletTab::UTXOs,
        Some(&state)
    ));
}

#[test]
fn utxo_age_refresh_is_visible_activity_scoped() {
    assert!(!should_refresh_utxo_ages(
        Activity::Wallet,
        WalletTab::Private,
        true
    ));
    assert!(!should_refresh_utxo_ages(
        Activity::Broadcaster,
        WalletTab::UTXOs,
        true
    ));
    assert!(!should_refresh_utxo_ages(
        Activity::Wallet,
        WalletTab::UTXOs,
        false
    ));
    assert!(should_refresh_utxo_ages(
        Activity::Wallet,
        WalletTab::UTXOs,
        true
    ));
}

#[test]
fn startup_settings_load_defaults_without_persisting() {
    let root = temp_wallet_db_root("startup-defaults");
    let store = DesktopVaultStore::open(root.clone()).expect("open wallet store");

    let settings = load_validated_startup_settings(&store).expect("load startup settings");

    assert_eq!(
        settings.poi.read_source,
        PoiReadSourceSetting::IndexedArtifacts
    );
    assert!(
        store
            .db()
            .get_app_settings_record(WALLET_SETTINGS_KEY)
            .expect("read settings record")
            .is_none()
    );

    drop(store);
    fs::remove_dir_all(root).expect("remove temp wallet db");
}

#[test]
fn startup_settings_invalid_record_is_recoverable_error() {
    let root = temp_wallet_db_root("startup-invalid");
    let store = DesktopVaultStore::open(root.clone()).expect("open wallet store");
    let mut settings = WalletSettings::default();
    for chain in settings.chains.per_chain.values_mut() {
        chain.enabled = false;
    }
    settings.walletconnect.project_id_override = Some("preserved-project-id".to_string());
    let payload = encode_wallet_settings(&settings).expect("encode invalid settings");
    store
        .db()
        .put_app_settings_record(WALLET_SETTINGS_KEY, &payload)
        .expect("write invalid settings");

    let editable = load_wallet_settings(store.db().as_ref())
        .expect("structurally valid settings remain available for repair");
    assert_eq!(editable, settings);
    assert_eq!(
        editable.walletconnect.project_id_override.as_deref(),
        Some("preserved-project-id")
    );
    assert!(editable.validate().is_err());

    let error = load_validated_startup_settings(&store).expect_err("settings should fail");
    let message = error.to_string();

    assert!(message.contains("wallet settings are invalid"));
    assert!(message.contains("at least one supported chain enabled"));

    drop(store);
    fs::remove_dir_all(root).expect("remove temp wallet db");
}

#[test]
fn startup_initial_chain_restores_enabled_remembered_chain() {
    assert_eq!(
        resolve_initial_chain_id(&[1, 9_007_199_254_740_993], Some(9_007_199_254_740_993)),
        9_007_199_254_740_993
    );
}

#[test]
fn startup_initial_chain_falls_back_when_remembered_chain_disabled() {
    assert_eq!(
        resolve_initial_chain_id(&[1, 56], Some(9_007_199_254_740_993)),
        1
    );
}

#[test]
fn wallet_app_options_preserve_cli_db_path() {
    let db_path = PathBuf::from("custom-wallet-db");
    let options = WalletAppOptions::try_from(crate::cli::Options {
        db_path: Some(db_path.clone()),
    })
    .expect("options");

    assert_eq!(options.db_path, db_path);
}

#[test]
fn locked_vault_screen_exposes_pre_unlock_settings_action() {
    assert!(should_show_pre_unlock_settings_action(
        &VaultState::CreateVault
    ));
    assert!(should_show_pre_unlock_settings_action(
        &VaultState::UnlockVault
    ));
    assert!(should_show_pre_unlock_settings_action(
        &VaultState::SetupWallet
    ));
    assert!(!should_show_pre_unlock_settings_action(
        &VaultState::SwitchingWallet
    ));
    assert!(!should_show_pre_unlock_settings_action(
        &VaultState::PendingSoftwareProfileOpen
    ));
    assert!(!should_show_pre_unlock_settings_action(
        &VaultState::ViewUnlocked
    ));
}

#[test]
fn switching_wallet_does_not_request_vault_input_focus() {
    assert!(!should_focus_vault_input(
        &VaultState::SwitchingWallet,
        WalletSetupMode::Choose,
    ));
}

#[test]
fn startup_pre_unlock_state_exposes_settings_and_error_recovery() {
    assert_eq!(
        startup_settings_action_state(true, true),
        super::StartupSettingsActionState {
            settings: true,
            reset: true,
            retry: true,
            maintenance_actions_enabled: true,
        }
    );
    assert_eq!(
        startup_settings_action_state(false, true),
        super::StartupSettingsActionState {
            settings: true,
            reset: false,
            retry: false,
            maintenance_actions_enabled: true,
        }
    );
    let blocked = startup_settings_action_state(true, false);
    assert!(blocked.reset);
    assert!(blocked.retry);
    assert!(!blocked.maintenance_actions_enabled);
}

#[test]
fn tor_bootstrap_recovery_rejects_stale_startup_generations_and_other_stages() {
    assert!(tor_bootstrap_recovery_is_current(
        7,
        7,
        WalletNetworkProgressStage::BootstrappingTor,
    ));
    assert!(!tor_bootstrap_recovery_is_current(
        6,
        7,
        WalletNetworkProgressStage::BootstrappingTor,
    ));
    assert!(!tor_bootstrap_recovery_is_current(
        7,
        7,
        WalletNetworkProgressStage::StartingTorBridge,
    ));
}

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
fn settings_discard_restores_saved_snapshot() {
    let mut saved = WalletSettings::default();
    saved.network.mode = NetworkModeSetting::Direct;
    let mut draft = saved.clone();
    draft.broadcaster.response_timeout_secs += 1;

    assert_ne!(draft, saved);
    assert_eq!(settings_draft_after_discard(&saved), saved);
}

#[test]
fn token_settings_display_includes_built_in_defaults() {
    let settings = WalletSettings::default();
    let entries = display_token_entries(&settings);

    let weth = entries
        .iter()
        .find(|entry| {
            entry.chain_id == 1
                && entry
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH default token");
    assert!(weth.built_in);
    assert_eq!(weth.symbol, "WETH");
    assert_eq!(weth.decimals, 18);
}

#[test]
fn price_anchor_settings_display_includes_built_in_defaults() {
    let settings = WalletSettings::default();
    let entries = display_price_anchor_entries(&settings);

    let weth = entries
        .iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH default price anchor");
    assert!(weth.built_in_default);
    assert_eq!(
        weth.price_anchor,
        PriceAnchorSettings::Fixed {
            rate: "1000000000000000000".to_string(),
        }
    );
    assert_eq!(
        settings.tokens.price_anchors,
        [] as [TokenPriceAnchorOverride; 0]
    );
}

#[test]
fn price_anchor_settings_display_overrides_built_in_defaults() {
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .price_anchors
        .push(TokenPriceAnchorOverride {
            key: TokenKey {
                chain_id: 1,
                token_address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
            },
            price_anchor: PriceAnchorSettings::Fixed {
                rate: "2000000000000000000".to_string(),
            },
        });

    let entries = display_price_anchor_entries(&settings);
    let weth = entries
        .iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH price anchor");

    assert!(!weth.built_in_default);
    assert_eq!(
        weth.price_anchor,
        PriceAnchorSettings::Fixed {
            rate: "2000000000000000000".to_string(),
        }
    );

    settings.tokens.price_anchors.clear();
    let entries = display_price_anchor_entries(&settings);
    let weth = entries
        .iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH default price anchor");
    assert!(weth.built_in_default);
}

#[test]
fn price_anchor_view_uses_token_symbol_when_available() {
    let settings = WalletSettings::default();
    let entries = display_price_anchor_entries(&settings);

    let weth = entries
        .iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH default price anchor");

    assert_eq!(weth.token_symbol.as_deref(), Some("WETH"));
    assert_eq!(price_anchor_token_primary_label(weth), "WETH");
}

#[test]
fn price_anchor_view_falls_back_to_short_address_without_symbol() {
    let token = Address::from([0x22; 20]);
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .price_anchors
        .push(TokenPriceAnchorOverride {
            key: TokenKey {
                chain_id: 1,
                token_address: token.to_string(),
            },
            price_anchor: PriceAnchorSettings::Fixed {
                rate: "1".to_string(),
            },
        });

    let entries = display_price_anchor_entries(&settings);
    let entry = entries
        .iter()
        .find(|entry| {
            entry
                .key
                .token_address
                .eq_ignore_ascii_case(&token.to_string())
        })
        .expect("unknown token price anchor");

    assert_eq!(entry.token_symbol, None);
    assert_eq!(
        price_anchor_token_primary_label(entry),
        railgun_ui::short_address(&token)
    );
}

#[test]
fn price_anchor_edit_prefills_dialog_values() {
    let settings = WalletSettings::default();
    let entries = display_price_anchor_entries(&settings);
    let weth = entries
        .iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH default price anchor");

    let values = price_anchor_dialog_values_from_entry(weth);

    assert_eq!(values.chain_id, 1);
    assert_eq!(values.token_address, weth.key.token_address);
    assert_eq!(values.anchor_type, "fixed");
    assert_eq!(values.fixed_rate, "1000000000000000000");
}

#[test]
fn price_anchor_edit_builtin_default_creates_sparse_override() {
    let mut settings = WalletSettings::default();
    let entry = display_price_anchor_entries(&settings)
        .into_iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH default price anchor");
    let edited = TokenPriceAnchorOverride {
        key: entry.key.clone(),
        price_anchor: PriceAnchorSettings::Fixed {
            rate: "3000000000000000000".to_string(),
        },
    };

    set_price_anchor_override(&mut settings, &entry, edited);

    assert_eq!(settings.tokens.price_anchors.len(), 1);
    let updated = display_price_anchor_entries(&settings)
        .into_iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH edited price anchor");
    assert!(!updated.built_in_default);
    assert_eq!(
        updated.price_anchor,
        PriceAnchorSettings::Fixed {
            rate: "3000000000000000000".to_string(),
        }
    );
}

#[test]
fn price_anchor_edit_override_replaces_existing_override() {
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .price_anchors
        .push(TokenPriceAnchorOverride {
            key: TokenKey {
                chain_id: 1,
                token_address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
            },
            price_anchor: PriceAnchorSettings::Fixed {
                rate: "2000000000000000000".to_string(),
            },
        });
    let entry = display_price_anchor_entries(&settings)
        .into_iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH override price anchor");

    set_price_anchor_override(
        &mut settings,
        &entry,
        TokenPriceAnchorOverride {
            key: entry.key.clone(),
            price_anchor: PriceAnchorSettings::Fixed {
                rate: "4000000000000000000".to_string(),
            },
        },
    );

    assert_eq!(settings.tokens.price_anchors.len(), 1);
    assert_eq!(
        settings.tokens.price_anchors[0].price_anchor,
        PriceAnchorSettings::Fixed {
            rate: "4000000000000000000".to_string(),
        }
    );
}

#[test]
fn price_anchor_add_dialog_values_create_override_without_mutating_settings() {
    let settings = WalletSettings::default();

    let anchor = price_anchor_override_from_dialog_values(&PriceAnchorDialogValues {
        chain_id: 42161,
        token_address: " 0x0000000000000000000000000000000000000002 ".to_string(),
        anchor_type: "oracle",
        fixed_rate: "1000000000000000000".to_string(),
        oracle_chain_id: 1,
        oracle_address: " 0x0000000000000000000000000000000000000003 ".to_string(),
        oracle_token_decimals: "6".to_string(),
        oracle_decimals: "8".to_string(),
        oracle_is_inversed: true,
        twap_pool_address: Address::ZERO.to_string(),
        twap_base_token_address: Address::ZERO.to_string(),
        twap_quote_token_address: Address::ZERO.to_string(),
        twap_base_token_decimals: "18".to_string(),
        twap_window_seconds: "1800".to_string(),
        product_scale_decimals: "18".to_string(),
        product_components: test_product_anchor_components(),
    })
    .expect("valid add price anchor dialog values");

    assert_eq!(
        settings.tokens.price_anchors,
        [] as [TokenPriceAnchorOverride; 0]
    );
    assert_eq!(anchor.key.chain_id, 42161);
    assert_eq!(
        anchor.key.token_address,
        "0x0000000000000000000000000000000000000002"
    );
    assert!(matches!(
        anchor.price_anchor,
        PriceAnchorSettings::Oracle {
            chain_id: 1,
            token_decimals: 6,
            oracle_decimals: 8,
            is_inversed: true,
            ref oracle_address,
            ..
        } if oracle_address == "0x0000000000000000000000000000000000000003"
    ));
}

#[test]
fn price_anchor_twap_dialog_round_trip_preserves_all_fields() {
    let twap = PriceAnchorSettings::UniswapV3Twap {
        pool_address: "0x0000000000000000000000000000000000000400".to_string(),
        base_token_address: "0x0000000000000000000000000000000000000401".to_string(),
        quote_token_address: "0x0000000000000000000000000000000000000402".to_string(),
        base_token_decimals: 8,
        window_seconds: 777,
    };
    let override_settings = TokenPriceAnchorOverride {
        key: TokenKey {
            chain_id: 1,
            token_address: "0x0000000000000000000000000000000000000002".to_string(),
        },
        price_anchor: twap.clone(),
    };
    let values = price_anchor_dialog_values_from_override(&override_settings);
    assert_eq!(values.anchor_type, "uniswap-v3-twap");
    assert_eq!(
        values.twap_pool_address,
        "0x0000000000000000000000000000000000000400"
    );
    assert_eq!(
        values.twap_base_token_address,
        "0x0000000000000000000000000000000000000401"
    );
    assert_eq!(
        values.twap_quote_token_address,
        "0x0000000000000000000000000000000000000402"
    );
    assert_eq!(values.twap_base_token_decimals, "8");
    assert_eq!(values.twap_window_seconds, "777");
    let round_trip = price_anchor_override_from_dialog_values(&values).expect("TWAP round trip");
    assert_eq!(round_trip.price_anchor, twap);
}

#[test]
fn price_anchor_add_dialog_values_create_product_override() {
    let twap_component = PriceAnchorSettings::UniswapV3Twap {
        pool_address: "0x0000000000000000000000000000000000000400".to_string(),
        base_token_address: "0x0000000000000000000000000000000000000401".to_string(),
        quote_token_address: "0x0000000000000000000000000000000000000402".to_string(),
        base_token_decimals: 6,
        window_seconds: 900,
    };
    let twap_component_values = price_anchor_component_dialog_values_from_anchor(&twap_component);
    let anchor = price_anchor_override_from_dialog_values(&PriceAnchorDialogValues {
        chain_id: 1,
        token_address: "0x0000000000000000000000000000000000000002".to_string(),
        anchor_type: "product",
        fixed_rate: "1000000000000000000".to_string(),
        oracle_chain_id: 1,
        oracle_address: "0x0000000000000000000000000000000000000003".to_string(),
        oracle_token_decimals: "18".to_string(),
        oracle_decimals: "8".to_string(),
        oracle_is_inversed: false,
        twap_pool_address: Address::ZERO.to_string(),
        twap_base_token_address: Address::ZERO.to_string(),
        twap_quote_token_address: Address::ZERO.to_string(),
        twap_base_token_decimals: "18".to_string(),
        twap_window_seconds: "1800".to_string(),
        product_scale_decimals: "12".to_string(),
        product_components: vec![
            PriceAnchorComponentDialogValues {
                anchor_type: "oracle",
                fixed_rate: "1000000000000000000".to_string(),
                oracle_chain_id: 42161,
                oracle_address: "0x0000000000000000000000000000000000000004".to_string(),
                oracle_token_decimals: "18".to_string(),
                oracle_decimals: "8".to_string(),
                oracle_is_inversed: false,
                twap_pool_address: Address::ZERO.to_string(),
                twap_base_token_address: Address::ZERO.to_string(),
                twap_quote_token_address: Address::ZERO.to_string(),
                twap_base_token_decimals: "18".to_string(),
                twap_window_seconds: "1800".to_string(),
            },
            twap_component_values,
        ],
    })
    .expect("valid product price anchor dialog values");

    assert!(matches!(
        anchor.price_anchor,
        PriceAnchorSettings::Product {
            scale_decimals: 12,
            ref components,
        } if matches!(
            components.as_slice(),
            [
                PriceAnchorSettings::Oracle {
                    chain_id: 42161,
                    oracle_decimals: 8,
                    is_inversed: false,
                    ..
                },
                PriceAnchorSettings::UniswapV3Twap {
                    pool_address,
                    base_token_address,
                    quote_token_address,
                    base_token_decimals: 6,
                    window_seconds: 900,
                },
            ]
            if pool_address == "0x0000000000000000000000000000000000000400"
                && base_token_address
                    == "0x0000000000000000000000000000000000000401"
                && quote_token_address
                    == "0x0000000000000000000000000000000000000402"
        )
    ));
}

#[test]
fn price_anchor_add_dialog_values_reject_invalid_anchor_type() {
    let err = price_anchor_override_from_dialog_values(&PriceAnchorDialogValues {
        chain_id: 1,
        token_address: "0x0000000000000000000000000000000000000002".to_string(),
        anchor_type: "bad",
        fixed_rate: "1000000000000000000".to_string(),
        oracle_chain_id: 1,
        oracle_address: "0x0000000000000000000000000000000000000003".to_string(),
        oracle_token_decimals: "18".to_string(),
        oracle_decimals: "8".to_string(),
        oracle_is_inversed: false,
        twap_pool_address: Address::ZERO.to_string(),
        twap_base_token_address: Address::ZERO.to_string(),
        twap_quote_token_address: Address::ZERO.to_string(),
        twap_base_token_decimals: "18".to_string(),
        twap_window_seconds: "1800".to_string(),
        product_scale_decimals: "18".to_string(),
        product_components: test_product_anchor_components(),
    })
    .expect_err("bad anchor type rejected");

    assert!(err.contains("fixed, oracle, or product"));
}

fn test_product_anchor_components() -> Vec<PriceAnchorComponentDialogValues> {
    vec![
        PriceAnchorComponentDialogValues {
            anchor_type: "oracle",
            fixed_rate: "1000000000000000000".to_string(),
            oracle_chain_id: 1,
            oracle_address: "0x0000000000000000000000000000000000000003".to_string(),
            oracle_token_decimals: "18".to_string(),
            oracle_decimals: "8".to_string(),
            oracle_is_inversed: false,
            twap_pool_address: Address::ZERO.to_string(),
            twap_base_token_address: Address::ZERO.to_string(),
            twap_quote_token_address: Address::ZERO.to_string(),
            twap_base_token_decimals: "18".to_string(),
            twap_window_seconds: "1800".to_string(),
        },
        PriceAnchorComponentDialogValues {
            anchor_type: "oracle",
            fixed_rate: "1000000000000000000".to_string(),
            oracle_chain_id: 1,
            oracle_address: "0x0000000000000000000000000000000000000004".to_string(),
            oracle_token_decimals: "18".to_string(),
            oracle_decimals: "8".to_string(),
            oracle_is_inversed: true,
            twap_pool_address: Address::ZERO.to_string(),
            twap_base_token_address: Address::ZERO.to_string(),
            twap_quote_token_address: Address::ZERO.to_string(),
            twap_base_token_decimals: "18".to_string(),
            twap_window_seconds: "1800".to_string(),
        },
    ]
}

#[test]
fn token_settings_display_applies_builtin_overrides_and_custom_tokens() {
    let custom = Address::from([0x77; 20]);
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .built_in_overrides
        .push(BuiltInTokenOverride {
            key: TokenKey {
                chain_id: 1,
                token_address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
            },
            symbol: Some("WETHx".to_string()),
            decimals: Some(17),
            icon_path: None,
            price_anchor: None,
        });
    settings.tokens.custom_tokens.push(CustomTokenSettings {
        chain_id: 1,
        token_address: custom.to_string(),
        symbol: "TST".to_string(),
        decimals: 4,
        icon_path: None,
        price_anchor: None,
    });

    let entries = display_token_entries(&settings);

    let overridden = entries
        .iter()
        .find(|entry| entry.chain_id == 1 && entry.symbol == "WETHx")
        .expect("overridden built-in token");
    assert!(overridden.built_in);
    assert_eq!(overridden.decimals, 17);

    let custom = entries
        .iter()
        .find(|entry| entry.chain_id == 1 && entry.symbol == "TST")
        .expect("custom token");
    assert!(!custom.built_in);
    assert_eq!(custom.decimals, 4);
}

#[gpui::test]
fn shared_chain_editor_commits_discards_resets_and_preserves_stale_settings(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::{
        AppContext as _, IntoElement, ParentElement as _, Render, Styled as _, VisualTestContext,
        div,
    };
    use railgun_ui::chain_editor::{ChainDraft, ChainEditorCommand, ChainField, RpcEndpointDraft};
    use std::{cell::RefCell, rc::Rc};

    /// Mounts the shared editor under `Root`, which hosts its confirmation dialogs.
    struct ChainEditorHost(Entity<ui::chain_editor::ChainEditor>);

    impl Render for ChainEditorHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    cx.update(gpui_component::init);
    let path = temp_wallet_db_root("shared-chain-editor");
    let store = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut saved = WalletSettings::default();
    saved.runtime.auto_lock_timeout_secs = Some(600);
    wallet_ops::settings::save_wallet_settings(store.db().as_ref(), &saved).unwrap();
    let owner_slot = Rc::new(RefCell::new(None));
    let slot = owner_slot.clone();
    let handle = cx.add_window(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let editor = cx.new(|cx| {
            WalletSettingsEditor::new(
                store.clone(),
                &path,
                runtime.handle().clone(),
                saved.clone(),
                maintenance,
                None,
                None,
                window,
                cx,
            )
        });
        let shared = editor.read(cx).chain_editor.clone();
        let host = cx.new(|_| ChainEditorHost(shared));
        *slot.borrow_mut() = Some(editor);
        gpui_component::Root::new(host, window, cx)
    });
    let owner = owner_slot.borrow_mut().take().unwrap();
    let cx = VisualTestContext::from_window(*handle, cx).into_mut();
    // The host has no scroll container, so the window must fit every built-in row plus the
    // custom chain at the bottom of the list.
    cx.simulate_resize(gpui::size(px(600.0), px(2000.0)));
    let click = |cx: &mut VisualTestContext, id: &'static str| {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let bounds = cx
            .debug_bounds(id)
            .unwrap_or_else(|| panic!("chain editor control {id} is rendered"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    };
    click(cx, "edit-chain-1");
    click(cx, "chain-enabled");
    click(cx, "chain-discard");
    assert_eq!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap(),
        saved
    );
    let stored = || wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap();
    // Done stages the chain in the Settings draft, and only the footer's commit persists it.
    let commit_staged = |cx: &mut VisualTestContext, expected: &WalletSettings| {
        cx.update(|_, cx| {
            owner.update(cx, |editor, cx| {
                assert!(editor.is_dirty());
                assert_eq!(&editor.draft, expected);
                assert!(editor.persist_draft(cx));
            });
        });
    };
    click(cx, "edit-chain-1");
    click(cx, "chain-enabled");
    click(cx, "chain-save");
    assert_eq!(stored(), saved);
    saved.chains.per_chain.get_mut(&1).unwrap().enabled = false;
    commit_staged(cx, &saved);
    assert_eq!(stored(), saved);
    // Done returns to the list; reopen the chain before resetting it.
    click(cx, "edit-chain-1");
    click(cx, "chain-reset");
    // Reset is destructive, so it lands only after the confirmation dialog is confirmed.
    assert!(
        cx.debug_bounds("dialog-layer").is_some(),
        "reset opens a confirmation dialog before changing anything"
    );
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(stored(), saved);
    saved.chains.per_chain.remove(&1);
    commit_staged(cx, &saved);
    assert_eq!(stored(), saved);
    // A newly direct endpoint waits for confirmation, and declining saves nothing.
    click(cx, "chain-endpoint-0-direct");
    click(cx, "chain-save");
    assert!(
        cx.debug_bounds("chain-direct-confirm").is_some(),
        "a newly direct endpoint asks for confirmation before saving"
    );
    assert!(cx.debug_bounds("chain-submit-direct-confirm").is_none());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("chain-direct-confirm").is_none());
    assert_eq!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap(),
        saved
    );
    click(cx, "chain-save");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(stored(), saved);
    cx.update(|_, cx| owner.update(cx, |editor, cx| assert!(editor.persist_draft(cx))));
    saved = stored();
    let endpoints = &saved.chains.per_chain[&1].rpc_endpoints;
    assert!(
        endpoints[0].direct && !endpoints[0].submit,
        "turning Direct on clears Submit"
    );
    assert!(
        endpoints[1..]
            .iter()
            .all(|endpoint| !endpoint.direct && endpoint.submit)
    );
    // Turning Submit back on for that Direct endpoint waits for its own confirmation.
    click(cx, "edit-chain-1");
    click(cx, "chain-endpoint-0-submit");
    click(cx, "chain-save");
    assert!(
        cx.debug_bounds("chain-submit-direct-confirm").is_some(),
        "submitting through a direct endpoint asks for confirmation before saving"
    );
    assert!(cx.debug_bounds("chain-direct-confirm").is_none());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap(),
        saved
    );
    // Reopen the confirmation so the host commit below lands while it is open.
    click(cx, "chain-save");
    assert!(cx.debug_bounds("chain-submit-direct-confirm").is_some());

    // Pricing can be saved without a network runtime and still preserves draft/revision isolation.
    cx.update(|window, cx| {
        owner.update(cx, |editor, cx| {
            let revision = wallet_ops::settings::settings_revision(&editor.saved)
                .unwrap()
                .to_string();
            let mut draft = wallet_ops::settings::chain_editor_draft(&editor.saved, 1).unwrap();
            draft.native_usd_pricing = railgun_ui::chain_editor::NativeUsdChoice::Disabled;
            let reply = editor
                .handle_chain_editor_command(
                    &revision,
                    &ChainEditorCommand::Save {
                        draft,
                        existing: true,
                    },
                    window,
                    cx,
                )
                .unwrap();
            assert!(!reply.restart_required);
            assert!(reply.draft.is_none());
            assert_eq!(
                editor.saved.chains.per_chain[&1].native_usd_pricing,
                wallet_ops::settings::NativeUsdPricing::Disabled
            );
        });
    });
    // Confirming the now-stale direct draft must not overwrite that concurrent commit.
    let concurrent = wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("chain-submit-direct-confirm").is_none());
    assert_eq!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap(),
        concurrent
    );
    // The draft stays open and stale, so Save is disabled rather than reopening the dialog.
    click(cx, "chain-save");
    assert!(cx.debug_bounds("chain-submit-direct-confirm").is_none());
    // A Test validates the draft like Save but persists nothing and publishes no status.
    cx.update(|_, cx| {
        owner.update(cx, |editor, cx| {
            let persisted =
                wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap();
            let before = editor.saved.clone();
            let mut draft = wallet_ops::settings::chain_editor_draft(&editor.saved, 1).unwrap();
            draft.native_usd_pricing = railgun_ui::chain_editor::NativeUsdChoice::Oracle;
            draft.fields.insert(
                ChainField::NativeUsdOracle,
                Address::repeat_byte(8).to_string(),
            );
            let target = editor.chain_editor.clone();
            editor.handle_chain_editor_probe(&draft, &target, cx);
            assert_eq!(editor.saved, before);
            assert_eq!(
                wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap(),
                persisted
            );
        });
    });
    // A narrow addition must keep an older unrelated Settings draft, and its old save must fail.
    cx.update(|window, cx| {
        owner.update(cx, |editor, cx| {
            editor.draft.runtime.auto_lock_timeout_secs = Some(900);
            let revision = wallet_ops::settings::settings_revision(&editor.saved)
                .unwrap()
                .to_string();
            let mut draft = ChainDraft::new();
            draft.chain_id = "9007199254740993".into();
            draft.native_usd_pricing = railgun_ui::chain_editor::NativeUsdChoice::Oracle;
            draft.fields.insert(
                ChainField::NativeUsdOracle,
                Address::repeat_byte(7).to_string(),
            );
            for (field, value) in [
                (ChainField::Name, "Custom"),
                (ChainField::NativeName, "Custom coin"),
                (ChainField::NativeSymbol, "CSTM"),
                (ChainField::NativeDecimals, "6"),
            ] {
                draft.fields.insert(field, value.into());
            }
            draft.rpc_endpoints = vec![RpcEndpointDraft::new(
                "https://synthetic:credential@rpc.example".into(),
            )];
            editor
                .handle_chain_editor_command(
                    &revision,
                    &ChainEditorCommand::Save {
                        draft,
                        existing: false,
                    },
                    window,
                    cx,
                )
                .unwrap();
            assert_eq!(editor.draft.runtime.auto_lock_timeout_secs, Some(900));
            assert!(
                editor
                    .saved
                    .chains
                    .custom
                    .contains_key(&9_007_199_254_740_993)
            );
            assert!(!editor.persist_draft(cx));
            assert_eq!(editor.saved.runtime.auto_lock_timeout_secs, Some(600));
            // A storage read failure must not advance the owner's saved/active revision.
            let before = editor.saved.clone();
            let record = store.db().list_app_settings_records("").unwrap().remove(0);
            store
                .db()
                .put_app_settings_record(&record.key, &[0xc1])
                .unwrap();
            let revision = wallet_ops::settings::settings_revision(&before)
                .unwrap()
                .to_string();
            assert!(
                editor
                    .handle_chain_editor_command(
                        &revision,
                        &ChainEditorCommand::Remove {
                            chain_id: "9007199254740993".into(),
                        },
                        window,
                        cx
                    )
                    .is_err()
            );
            assert_eq!(editor.saved, before);
            assert_eq!(
                store
                    .db()
                    .get_app_settings_record(&record.key)
                    .unwrap()
                    .unwrap(),
                vec![0xc1]
            );
            store
                .db()
                .put_app_settings_record(&record.key, &record.payload)
                .unwrap();
            // The draft never sees outside commits, so dropping it picks up the addition.
            editor.discard_changes(cx);
        });
    });
    assert!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref())
            .unwrap()
            .chains
            .custom
            .contains_key(&9_007_199_254_740_993)
    );
    // Leaving the stale draft returns the shared list, which already carries that addition.
    click(cx, "chain-discard");
    assert!(
        cx.debug_bounds("edit-chain-9007199254740993").is_some(),
        "a commit made outside the editor refreshes the shared chain list"
    );
    click(cx, "edit-chain-9007199254740993");
    click(cx, "chain-save");
    assert!(
        cx.debug_bounds("edit-chain-9007199254740993").is_some(),
        "saving a custom chain returns to the list"
    );

    let endpoint = |settings: &WalletSettings, chain_id: u64, index: usize| {
        wallet_ops::settings::chain_editor_draft(settings, chain_id)
            .unwrap()
            .rpc_endpoints
            .swap_remove(index)
    };
    // Two built-in chains stage separately and persist in one footer commit.
    let before = stored();
    for chain in ["edit-chain-1", "edit-chain-56"] {
        click(cx, chain);
        click(cx, "chain-endpoint-1-general");
        click(cx, "chain-save");
    }
    assert_eq!(stored(), before);
    cx.update(|_, cx| owner.update(cx, |editor, cx| assert!(editor.persist_draft(cx))));
    saved = stored();
    for chain_id in [1, 56] {
        assert!(endpoint(&before, chain_id, 1).general);
        assert!(!endpoint(&saved, chain_id, 1).general);
    }

    // An Add chain form opened after an unrelated draft edit stages against the new draft.
    cx.update(|_, cx| {
        owner.update(cx, |editor, cx| {
            editor.draft.runtime.auto_lock_timeout_secs = Some(900);
            editor.draft_changed(cx);
        });
    });
    click(cx, "chain-add");
    cx.simulate_input("424242");
    for value in ["Added", "Added coin", "ADD", "18"] {
        cx.simulate_keystrokes("tab");
        cx.simulate_input(value);
    }
    click(cx, "chain-add-endpoint");
    cx.simulate_input("https://added.example");
    click(cx, "chain-save");
    assert!(
        cx.debug_bounds("edit-chain-424242").is_some(),
        "the staged chain is listed"
    );
    click(cx, "edit-chain-1");
    click(cx, "chain-endpoint-1-sensitive");
    click(cx, "chain-save");
    assert_eq!(stored(), saved);
    cx.update(|_, cx| {
        owner.update(cx, |editor, cx| {
            assert_eq!(editor.draft.runtime.auto_lock_timeout_secs, Some(900));
            assert!(editor.draft.chains.custom.contains_key(&424_242));
            assert!(!endpoint(&editor.draft, 1, 1).sensitive);
            editor.discard_changes(cx);
        });
    });
    // After Settings discard the list and the reopened chain show saved values again, so an
    // untouched Done stages nothing.
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("edit-chain-424242").is_none());
    click(cx, "edit-chain-1");
    click(cx, "chain-save");
    cx.update(|_, cx| {
        owner.update(cx, |editor, _| {
            assert!(!editor.is_dirty());
            assert_eq!(editor.draft, saved);
        });
    });

    // An outside commit leaves staged edits staging, and the footer then reports the conflict.
    click(cx, "edit-chain-1");
    click(cx, "chain-endpoint-1-sensitive");
    click(cx, "chain-save");
    cx.update(|_, cx| {
        owner.update(cx, |editor, cx| {
            let revision = wallet_ops::settings::settings_revision(&editor.saved).unwrap();
            let mut draft = wallet_ops::settings::chain_editor_draft(&editor.saved, 137).unwrap();
            draft.native_usd_pricing = railgun_ui::chain_editor::NativeUsdChoice::Disabled;
            let candidate = wallet_ops::settings::chain_editor_mutation(
                &editor.saved,
                &ChainEditorCommand::Save {
                    draft,
                    existing: true,
                },
            )
            .unwrap()
            .prepare(&editor.saved)
            .unwrap();
            editor
                .commit_chain_candidate(revision, candidate, cx)
                .unwrap();
        });
    });
    let external = stored();
    assert_ne!(external, saved);
    click(cx, "edit-chain-56");
    click(cx, "chain-endpoint-1-sensitive");
    click(cx, "chain-save");
    cx.update(|_, cx| {
        owner.update(cx, |editor, cx| {
            assert!(!endpoint(&editor.draft, 1, 1).sensitive);
            assert!(!endpoint(&editor.draft, 56, 1).sensitive);
            assert!(!editor.persist_draft(cx));
            editor.discard_changes(cx);
        });
    });
    assert_eq!(stored(), external);
    cx.simulate_resize(gpui::size(px(360.0), px(480.0)));
    cx.update(|window, cx| {
        window.set_rem_size(px(22.0));
        window.draw(cx).clear(cx);
    });
    // The fixed footer remains reachable when the advanced field body overflows.
    click(cx, "edit-chain-1");
    click(cx, "chain-discard");
    // Dropping the window and entities releases the test database before removal.
    cx.update(|window, _| window.remove_window());
    drop(owner);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
#[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
fn settings_fields_commit_first_edit_after_sync_and_number_steps(cx: &mut gpui::TestAppContext) {
    use gpui::{
        AppContext as _, IntoElement, ParentElement as _, Render, Styled as _, VisualTestContext,
        div,
    };
    use gpui_component::setting::{
        NumberFieldOptions, SettingGroup, SettingItem, SettingPage, Settings as ComponentSettings,
    };
    use std::{cell::RefCell, rc::Rc};

    /// Renders one number field followed by one string field, so the string input is the
    /// window's last tab stop and the number input the one before it.
    struct FieldsHost(Entity<WalletSettingsEditor>);

    impl Render for FieldsHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            let timeout = WalletSettingsEditor::number_field(
                "test-response-timeout",
                self.0.clone(),
                NumberFieldOptions {
                    min: 1.0,
                    max: 86_400.0,
                    step: 1.0,
                },
                None,
                |settings| settings.broadcaster.response_timeout_secs as f64,
                |settings, value| settings.broadcaster.response_timeout_secs = value as u64,
            );
            let nwaku = WalletSettingsEditor::shared_string_field(
                "test-nwaku-url",
                self.0.clone(),
                |settings| settings.waku.nwaku_url.clone().unwrap_or_default(),
                |settings, value| settings.waku.nwaku_url = Some(value),
            );
            div().size_full().child(
                ComponentSettings::new("test-settings-fields").page(
                    SettingPage::new("Fields").group(
                        SettingGroup::new()
                            .item(SettingItem::new("Timeout", timeout))
                            .item(SettingItem::new("nwaku", nwaku)),
                    ),
                ),
            )
        }
    }

    cx.update(gpui_component::init);
    let path = temp_wallet_db_root("settings-field-sync");
    let store = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let owner_slot = Rc::new(RefCell::new(None));
    let slot = owner_slot.clone();
    let handle = cx.add_window(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let editor = cx.new(|cx| {
            WalletSettingsEditor::new(
                store.clone(),
                &path,
                runtime.handle().clone(),
                WalletSettings::default(),
                maintenance,
                None,
                None,
                window,
                cx,
            )
        });
        let host = cx.new(|_| FieldsHost(editor.clone()));
        *slot.borrow_mut() = Some(editor);
        gpui_component::Root::new(host, window, cx)
    });
    let owner = owner_slot.borrow_mut().take().unwrap();
    let cx = VisualTestContext::from_window(*handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(1000.0), px(700.0)));
    let redraw = |cx: &mut VisualTestContext| {
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
    };
    redraw(cx);

    // A programmatic draft change rewrites both inputs without emitting edits.
    cx.update(|_, cx| {
        owner.update(cx, |editor, cx| {
            editor.draft.broadcaster.response_timeout_secs = 10;
            editor.draft.waku.nwaku_url = Some("http://a".to_owned());
            editor.programmatic_draft_changed(cx);
        });
    });
    redraw(cx);

    // The user's first keystroke after that sync must reach the draft.
    cx.update(gpui::Window::focus_prev);
    cx.simulate_input("b");
    redraw(cx);
    let nwaku_url = cx.update(|_, cx| owner.read(cx).draft.waku.nwaku_url.clone());
    assert_eq!(nwaku_url.as_deref(), Some("http://ab"));

    cx.update(gpui::Window::focus_prev);
    cx.simulate_input("5");
    redraw(cx);
    assert_eq!(
        cx.update(|_, cx| owner.read(cx).draft.broadcaster.response_timeout_secs),
        105
    );

    // Stepping the number input updates the draft, not only the visible text.
    cx.simulate_keystrokes("up");
    redraw(cx);
    assert_eq!(
        cx.update(|_, cx| owner.read(cx).draft.broadcaster.response_timeout_secs),
        106
    );

    // Dropping the window and entities releases the test database before removal.
    cx.update(|window, _| window.remove_window());
    drop(owner);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn tron_token_addresses_are_entered_and_shown_in_base58() {
    let tron = railgun_ui::TRON_CHAIN_ID;
    let usdt = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";
    let stored = stored_token_address(tron, usdt).unwrap();
    assert!(stored.eq_ignore_ascii_case("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c"));
    assert_eq!(display_token_address(tron, &stored), usdt);
    assert_eq!(
        stored_token_address(tron, "0xa614f803b6fd780986a42c78ec9c7f77e6ded13c"),
        Err(TRON_ADDRESS_NEEDED.to_owned())
    );
}

#[gpui::test]
fn tron_settings_refuse_submit_without_an_http_api_url(cx: &mut gpui::TestAppContext) {
    use gpui::{
        AppContext as _, IntoElement, ParentElement as _, Render, Styled as _, VisualTestContext,
        div,
    };
    use gpui_component::setting::{SettingPage, Settings as ComponentSettings};
    use std::{cell::RefCell, rc::Rc};
    use wallet_ops::settings::TronEndpointSetting;

    struct TronHost(Entity<WalletSettingsEditor>);

    impl Render for TronHost {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            let page =
                SettingPage::new("Tron").groups(self.0.read(cx).tron_settings_groups(&self.0));
            div()
                .size_full()
                .child(ComponentSettings::new("test-tron-settings").page(page))
        }
    }

    cx.update(gpui_component::init);
    let path = temp_wallet_db_root("tron-settings-submit");
    let store = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut saved = WalletSettings::default();
    let mut endpoints = saved.tron.effective_endpoints();
    endpoints.insert(
        0,
        TronEndpointSetting {
            json_rpc_url: "https://json-rpc-only.example/jsonrpc".to_owned(),
            submit: false,
            ..TronEndpointSetting::default()
        },
    );
    saved.tron.endpoints = Some(endpoints);
    saved.validate().unwrap();
    let owner_slot = Rc::new(RefCell::new(None));
    let slot = owner_slot.clone();
    let handle = cx.add_window(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let editor = cx.new(|cx| {
            WalletSettingsEditor::new(
                store.clone(),
                &path,
                runtime.handle().clone(),
                saved.clone(),
                maintenance,
                None,
                None,
                window,
                cx,
            )
        });
        let host = cx.new(|_| TronHost(editor.clone()));
        *slot.borrow_mut() = Some(editor);
        gpui_component::Root::new(host, window, cx)
    });
    let owner = owner_slot.borrow_mut().take().unwrap();
    let cx = VisualTestContext::from_window(*handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(1000.0), px(1200.0)));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let bounds = cx
        .debug_bounds("tron-endpoint-0-submit")
        .expect("the JSON-RPC-only endpoint's Submit toggle is rendered");
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    cx.run_until_parked();

    let (draft, status) = cx.update(|_, cx| {
        let editor = owner.read(cx);
        (editor.draft.tron.clone(), editor.status.clone())
    });
    assert_eq!(
        draft, saved.tron,
        "Submit stays off without an HTTP API URL"
    );
    assert!(
        status.is_none(),
        "the refusal stays in the row, not the shared page banner"
    );
    assert!(
        cx.debug_bounds("tron-endpoint-0-no-http-api").is_some(),
        "the JSON-RPC-only row states why it can't submit"
    );
    assert!(
        cx.debug_bounds("tron-endpoint-1-no-http-api").is_none(),
        "a row with an HTTP API URL states no refusal"
    );

    // Dropping the window and entities releases the test database before removal.
    cx.update(|window, _| window.remove_window());
    drop(owner);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn data_folder_size_resets_after_maintenance_and_ignores_stale_walks(
    cx: &mut gpui::TestAppContext,
) {
    use crate::root::settings::DataFolderSize;
    use gpui::{AppContext as _, IntoElement, Render, div};
    use std::{cell::RefCell, rc::Rc};

    struct EmptyHost;

    impl Render for EmptyHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div()
        }
    }

    cx.update(gpui_component::init);
    let path = temp_wallet_db_root("data-folder-size");
    let store = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let owner_slot = Rc::new(RefCell::new(None));
    let slot = owner_slot.clone();
    let handle = cx.add_window(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let editor = cx.new(|cx| {
            WalletSettingsEditor::new(
                store.clone(),
                &path,
                runtime.handle().clone(),
                WalletSettings::default(),
                maintenance.clone(),
                None,
                None,
                window,
                cx,
            )
        });
        *slot.borrow_mut() = Some((editor, maintenance));
        EmptyHost
    });
    let (editor, maintenance) = owner_slot.borrow_mut().take().unwrap();

    // A reset that finishes makes a measured size stale.
    editor.update(cx, |editor, _cx| {
        editor.data_folder_size = DataFolderSize::Ready(1);
    });
    assert!(maintenance.update(cx, |controller, cx| {
        controller.start_merkle_reset(store.as_ref(), cx)
    }));
    assert!(!maintenance.read_with(cx, |controller, _cx| controller.is_idle()));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !maintenance.read_with(cx, |controller, _cx| controller.is_idle()) {
        assert!(std::time::Instant::now() < deadline, "the reset finishes");
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
        cx.run_until_parked();
    }
    assert_eq!(
        editor.read_with(cx, |editor, _cx| editor.data_folder_size),
        DataFolderSize::Unknown
    );

    // A walk superseded by a later one cannot overwrite it.
    editor.update(cx, |editor, cx| {
        editor.data_folder_size_generation = 2;
        editor.data_folder_size = DataFolderSize::Calculating(2);
        editor.finish_data_folder_size(1, DataFolderSize::Ready(99), cx);
        assert_eq!(editor.data_folder_size, DataFolderSize::Calculating(2));
    });

    handle
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    drop(editor);
    drop(maintenance);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn pre_start_editor_saves_network_changes_that_save_otherwise_refuses(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::{AppContext as _, IntoElement, Render, div};
    use std::{cell::RefCell, rc::Rc};

    struct EmptyHost;

    impl Render for EmptyHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div()
        }
    }

    cx.update(gpui_component::init);
    let path = temp_wallet_db_root("pre-start-save");
    let store = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let owner_slot = Rc::new(RefCell::new(None));
    let slot = owner_slot.clone();
    let handle = cx.add_window(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let editor = cx.new(|cx| {
            WalletSettingsEditor::new(
                store.clone(),
                &path,
                runtime.handle().clone(),
                WalletSettings::default(),
                maintenance,
                None,
                None,
                window,
                cx,
            )
        });
        *slot.borrow_mut() = Some(editor);
        EmptyHost
    });
    let editor = owner_slot.borrow_mut().take().unwrap();
    let stored_mode = || {
        wallet_ops::settings::load_wallet_settings(store.db().as_ref())
            .unwrap()
            .network
            .mode
    };

    editor.update(cx, |editor, cx| {
        editor.set_draft_network_mode(NetworkModeSetting::Direct, cx);
        assert!(!editor.save_draft(cx), "a running wallet must Apply this");
    });
    assert_eq!(stored_mode(), NetworkModeSetting::Tor);
    editor.update(cx, |editor, cx| {
        editor.set_pre_start(cx);
        assert!(editor.save_draft(cx));
    });
    assert_eq!(stored_mode(), NetworkModeSetting::Direct);

    handle
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    drop(editor);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn poi_publisher_choice_follows_draft_and_keeps_gateways(cx: &mut gpui::TestAppContext) {
    use gpui::{AppContext as _, IntoElement, Render, div};
    use std::{cell::RefCell, rc::Rc};
    use wallet_ops::settings::PoiArtifactSettings;

    struct EmptyHost;

    impl Render for EmptyHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            div()
        }
    }

    cx.update(gpui_component::init);
    let path = temp_wallet_db_root("poi-publisher-choice");
    let store = Arc::new(DesktopVaultStore::open(path.clone()).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let official = PoiArtifactSettings::official_preset();
    let custom_gateways = vec!["https://gateway.example".to_owned()];
    let mut saved = WalletSettings::default();
    saved.poi.artifact.publisher_pubkey = "11".repeat(32);
    saved.poi.artifact.gateway_urls.clone_from(&custom_gateways);
    let owner_slot = Rc::new(RefCell::new(None));
    let slot = owner_slot.clone();
    let handle = cx.add_window(|window, cx| {
        let maintenance = cx.new(|_| WalletMaintenanceController::new(runtime.handle().clone()));
        let editor = cx.new(|cx| {
            WalletSettingsEditor::new(
                store.clone(),
                &path,
                runtime.handle().clone(),
                saved.clone(),
                maintenance,
                None,
                None,
                window,
                cx,
            )
        });
        *slot.borrow_mut() = Some(editor);
        EmptyHost
    });
    let editor = owner_slot.borrow_mut().take().unwrap();

    editor.update(cx, |editor, cx| {
        // A stored publisher that differs from the official preset reads as Custom.
        assert!(editor.poi_custom_publisher_selected());

        // Official restores the publisher identity and keeps the user's gateways.
        editor.select_poi_publisher(false, cx);
        assert!(!editor.poi_custom_publisher_selected());
        let artifact = &editor.draft.poi.artifact;
        assert_eq!(artifact.publisher_pubkey, official.publisher_pubkey);
        assert_eq!(artifact.manifest_source, official.manifest_source);
        assert_eq!(artifact.gateway_urls, custom_gateways);

        // Custom stays selected while its values still equal the official preset.
        editor.select_poi_publisher(true, cx);
        assert!(editor.poi_custom_publisher_selected());

        // Discarding to official saved values clears the editor-only choice.
        editor.saved = WalletSettings::default();
        editor.discard_changes(cx);
        assert!(!editor.poi_custom_publisher_selected());
    });

    handle
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    drop(editor);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn chains_page_keywords_cover_listed_chains_and_editor_fields() {
    use crate::root::settings::chains_page_keywords;
    use railgun_ui::chain_editor::ChainField;
    use wallet_ops::settings::{
        ChainContractSettings, ChainGasSettings, CustomChainSettings, NativeCurrency,
        NativeUsdPricing,
    };

    let mut draft = WalletSettings::default();
    draft.chains.custom.insert(
        999_999,
        CustomChainSettings {
            native_usd_pricing: NativeUsdPricing::default(),
            name: "My devnet".to_owned(),
            native_currency: NativeCurrency {
                name: "Dev".to_owned(),
                symbol: "DEV".to_owned(),
                decimals: 18,
            },
            rpc_endpoints: vec!["https://rpc.example".to_owned().into()],
            explorer_urls: Vec::new(),
            enabled: true,
            contracts: ChainContractSettings::default(),
            finality_depth: None,
            gas: ChainGasSettings::default(),
        },
    );
    let keywords = chains_page_keywords(&draft);
    let has = |keyword: &str| {
        keywords
            .iter()
            .any(|candidate| candidate.as_ref() == keyword)
    };

    let arbitrum = railgun_ui::chain_name(42161).expect("Arbitrum is built in");
    assert!(has(arbitrum) && has("42161"), "built-in chain name and ID");
    assert!(
        has("My devnet") && has("999999"),
        "a custom chain that exists only in the draft"
    );
    for field in ChainField::ALL {
        assert!(has(field.label()), "chain editor field {field:?}");
    }
}

#[gpui::test]
fn chain_background_activation_is_demand_based_sticky_and_stops_on_retirement(
    cx: &mut gpui::TestAppContext,
) {
    let path = temp_wallet_db_root("retired-root-warmup");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let wallet = super::public_accounts::fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    cx.update(|window, cx| {
        root.update(cx, |root, cx| {
            let private = root.effective_chain_configs.get(1).unwrap().clone();
            let public = |chain_id, enabled| {
                let mut chain = private.clone();
                chain.chain_id = chain_id;
                chain.enabled = enabled;
                chain.railgun = None;
                chain.rpc_route =
                    wallet_ops::RpcChainRoute::new(chain_id, Vec::<reqwest::Url>::new());
                chain
            };
            root.effective_chain_configs = [
                private.clone(),
                public(10, true),
                public(20, true),
                public(30, false),
            ]
            .into_iter()
            .collect();
            root.selected_network = crate::root::SelectedNetwork::Evm(10);
            root.last_evm_chain = 10;
            root.view_session = None;
            root.install_chain_background();
            assert_eq!(root.active_rpc_chains, [1, 10].into_iter().collect());
            root.select_chain(20, window, cx);
            root.select_chain(10, window, cx);
            assert_eq!(root.active_rpc_chains, [1, 10, 20].into_iter().collect());
            root.install_chain_background();
            root.activate_chain_background(30);
            assert_eq!(root.active_rpc_chains, [1, 10, 20].into_iter().collect());
            assert!(root.rpc_identity_warmup.is_some());
            drop(root.begin_root_replacement_shutdown(cx));
            assert!(root.rpc_identity_warmup.is_none());
            assert!(root.active_rpc_chains.is_empty());
            // Settings Apply can defer work on the outgoing root, after retirement.
            root.apply_saved_request_settings(&WalletSettings::default(), cx);
            root.activate_chain_background(1);
            assert!(root.rpc_identity_warmup.is_none());
            assert!(root.active_rpc_chains.is_empty());
        });
    });
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    drop(entered);
    drop(runtime);
    std::fs::remove_dir_all(path).unwrap();
}
