use super::*;
use crate::root::maintenance::{
    PublicSyncResetCompletion, WalletMaintenanceReset, WalletMaintenanceStateMachine,
    public_sync_reset_restart_is_safe,
};
use crate::root::settings::{
    apply_indexed_artifact_source_mode, indexed_artifact_source_mode_value,
    indexed_artifact_source_status_message, price_anchor_component_dialog_values_from_anchor,
    price_anchor_dialog_values_from_override, should_show_indexed_artifact_custom_settings,
};
use crate::root::startup::tor_bootstrap_recovery_is_current;
use wallet_ops::WalletNetworkProgressStage;
use wallet_ops::settings::{
    IndexedArtifactSettings, IndexedArtifactSourceModeSetting,
    default_sponsored_bundle_relay_endpoints,
};

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
    let state = ChainUtxoState::Loading { progress: None };

    assert!(!should_focus_utxo_table(
        Activity::Wallet,
        WalletTab::Private,
        Some(&state)
    ));
    assert!(!should_focus_utxo_table(
        Activity::Broadcaster,
        WalletTab::Activity,
        Some(&state)
    ));
    assert!(should_focus_utxo_table(
        Activity::Wallet,
        WalletTab::Activity,
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
        WalletTab::Activity,
        true
    ));
    assert!(!should_refresh_utxo_ages(
        Activity::Wallet,
        WalletTab::Activity,
        false
    ));
    assert!(should_refresh_utxo_ages(
        Activity::Wallet,
        WalletTab::Activity,
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
    assert_eq!(resolve_initial_chain_id(&[1, 56, 137], Some(137)), 137);
}

#[test]
fn startup_initial_chain_falls_back_when_remembered_chain_disabled() {
    assert_eq!(resolve_initial_chain_id(&[1, 56], Some(137)), 1);
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
fn primary_sidebar_order_places_proposals_before_settings() {
    assert_eq!(
        sidebar_primary_activity_order(),
        [
            Activity::Wallet,
            Activity::Broadcaster,
            Activity::AddressBook,
            Activity::Proposals,
            Activity::Settings
        ]
    );
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
    assert!(display_waku_doh_fallback_endpoints(&direct).is_empty());

    let mut proxy = settings.clone();
    proxy.network.mode = NetworkModeSetting::Proxy;
    proxy.network.proxy_url = Some("socks5h://127.0.0.1:9050".to_string());
    assert_eq!(display_waku_doh_endpoint(&proxy), DEFAULT_DOH_ENDPOINT);
    assert!(display_waku_doh_fallback_endpoints(&proxy).is_empty());

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
    assert!(display_waku_doh_fallback_endpoints(&settings).is_empty());

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
    assert!(display_waku_dns_enr_trees(&settings).is_empty());

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
fn waku_direct_peer_settings_mutations_update_rows() {
    let mut settings = WalletSettings::default();
    assert_eq!(
        display_waku_direct_peers(&settings),
        default_waku_direct_peers()
    );
    assert!(settings.waku.direct_peers.is_none());

    remove_waku_direct_peer(&mut settings, 0);
    assert_eq!(settings.waku.direct_peers, Some(Vec::new()));
    assert!(display_waku_direct_peers(&settings).is_empty());

    let first = WakuDirectPeerSetting {
        peer_id: "16Uiu2HAkwNeQVY32bUrL1eM68ryMa48PXY5Bhfxfg9e2byYcc46m".to_string(),
        addr: "/dns4/prod.rootedinprivacy.com/tcp/30304/p2p/16Uiu2HAkwNeQVY32bUrL1eM68ryMa48PXY5Bhfxfg9e2byYcc46m".to_string(),
    };
    let edited = WakuDirectPeerSetting {
        peer_id: first.peer_id.clone(),
        addr: "/dns4/prod.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAkwNeQVY32bUrL1eM68ryMa48PXY5Bhfxfg9e2byYcc46m".to_string(),
    };

    add_waku_direct_peer(&mut settings, first);
    assert_eq!(display_waku_direct_peers(&settings).len(), 1);

    set_waku_direct_peer(&mut settings, 0, edited.clone());
    assert_eq!(settings.waku.direct_peers, Some(vec![edited]));

    remove_waku_direct_peer(&mut settings, 0);
    assert_eq!(settings.waku.direct_peers, Some(Vec::new()));
}

#[test]
fn chain_rpc_settings_display_presets_until_customized() {
    let settings = WalletSettings::default();
    for chain_id in railgun_ui::DEFAULT_CHAINS {
        assert_eq!(
            display_chain_rpc_endpoints(&settings, *chain_id),
            default_chain_rpc_endpoints(*chain_id).expect("supported chain preset")
        );
        assert!(
            settings
                .chains
                .per_chain
                .get(chain_id)
                .is_some_and(|chain| chain.rpc_endpoints.is_empty())
        );
    }

    let mut custom = settings;
    custom.chains.per_chain.entry(1).or_default().rpc_endpoints = vec![
        "https://rpc-one.example".to_string(),
        "https://rpc-two.example".to_string(),
    ];

    assert_eq!(
        display_chain_rpc_endpoints(&custom, 1),
        vec![
            "https://rpc-one.example".to_string(),
            "https://rpc-two.example".to_string()
        ]
    );
}

#[test]
fn chain_rpc_settings_mutations_materialize_presets() {
    let mut settings = WalletSettings::default();
    let defaults = default_chain_rpc_endpoints(1).expect("supported chain preset");

    set_chain_rpc_endpoint(&mut settings, 1, 0, " https://custom-rpc.example ");
    let endpoints = &settings
        .chains
        .per_chain
        .get(&1)
        .expect("chain settings")
        .rpc_endpoints;
    assert_eq!(endpoints.len(), defaults.len());
    assert_eq!(endpoints[0], "https://custom-rpc.example");

    add_chain_rpc_endpoint(&mut settings, 1, " https://added-rpc.example ");
    let endpoints = &settings
        .chains
        .per_chain
        .get(&1)
        .expect("chain settings")
        .rpc_endpoints;
    assert_eq!(endpoints.len(), defaults.len() + 1);
    assert_eq!(endpoints.last().unwrap(), "https://added-rpc.example");

    remove_chain_rpc_endpoint(&mut settings, 1, 1);
    assert_eq!(
        settings
            .chains
            .per_chain
            .get(&1)
            .expect("chain settings")
            .rpc_endpoints
            .len(),
        defaults.len()
    );
}

#[test]
fn chain_rpc_settings_remove_default_creates_custom_override() {
    let mut settings = WalletSettings::default();
    let defaults = default_chain_rpc_endpoints(1).expect("supported chain preset");

    remove_chain_rpc_endpoint(&mut settings, 1, 0);

    let expected = defaults.into_iter().skip(1).collect::<Vec<_>>();
    assert_eq!(display_chain_rpc_endpoints(&settings, 1), expected);
}

#[test]
fn sponsored_relay_settings_preserve_default_override_and_disabled_states() {
    let mut settings = WalletSettings::default();
    let defaults = default_sponsored_bundle_relay_endpoints(1);
    assert_eq!(display_sponsored_bundle_relays(&settings, 1), defaults);
    assert_eq!(
        settings
            .chains
            .per_chain
            .get(&1)
            .expect("ethereum settings")
            .sponsored_bundle_relays,
        None
    );

    set_sponsored_bundle_relay(&mut settings, 1, 0, " https://custom-relay.example ");
    assert_eq!(
        display_sponsored_bundle_relays(&settings, 1)[0],
        "https://custom-relay.example"
    );
    while !display_sponsored_bundle_relays(&settings, 1).is_empty() {
        remove_sponsored_bundle_relay(&mut settings, 1, 0);
    }
    assert_eq!(
        display_sponsored_bundle_relays(&settings, 1),
        Vec::<String>::new()
    );
    assert_eq!(
        settings
            .chains
            .per_chain
            .get(&1)
            .expect("ethereum settings")
            .sponsored_bundle_relays,
        Some(Vec::new())
    );
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
fn chain_quick_sync_setting_displays_preset_until_customized() {
    let settings = WalletSettings::default();
    for chain_id in railgun_ui::DEFAULT_CHAINS {
        assert_eq!(
            display_chain_quick_sync_endpoint(&settings, *chain_id),
            default_chain_quick_sync_endpoint(*chain_id).unwrap_or_default()
        );
    }

    let mut custom = settings;
    custom
        .chains
        .per_chain
        .entry(1)
        .or_default()
        .quick_sync
        .endpoint = Some("https://quick.example/graphql".to_string());

    assert_eq!(
        display_chain_quick_sync_endpoint(&custom, 1),
        "https://quick.example/graphql"
    );
}

#[test]
fn chain_contract_settings_display_presets_until_customized() {
    let settings = WalletSettings::default();
    for chain_id in railgun_ui::DEFAULT_CHAINS {
        assert_eq!(
            display_chain_contract_settings(&settings, *chain_id),
            default_chain_contract_settings(*chain_id).expect("supported chain preset")
        );
        assert!(
            settings
                .chains
                .per_chain
                .get(chain_id)
                .is_some_and(|chain| chain.contracts.railgun_contract.is_none())
        );
    }

    let mut custom = settings;
    custom
        .chains
        .per_chain
        .entry(1)
        .or_default()
        .contracts
        .multicall_contract = Some("0x0000000000000000000000000000000000000001".to_string());

    let displayed = display_chain_contract_settings(&custom, 1);
    let defaults = default_chain_contract_settings(1).expect("ethereum preset");
    assert_eq!(displayed.railgun_contract, defaults.railgun_contract);
    assert_eq!(displayed.coinbase_payer, defaults.coinbase_payer);
    assert_eq!(
        displayed.multicall_contract.as_deref(),
        Some("0x0000000000000000000000000000000000000001")
    );
}

#[test]
fn settings_discard_reverts_relay_adapter_7702_display_value() {
    let saved = WalletSettings::default();
    let mut draft = saved.clone();
    draft
        .chains
        .per_chain
        .entry(1)
        .or_default()
        .contracts
        .relay_adapt_7702_contract = Some("0x0000000000000000000000000000000000000001".to_string());
    assert_eq!(
        display_chain_contract_settings(&draft, 1)
            .relay_adapt_7702_contract
            .as_deref(),
        Some("0x0000000000000000000000000000000000000001")
    );

    let discarded = settings_draft_after_discard(&saved);
    assert_eq!(
        display_chain_contract_settings(&discarded, 1).relay_adapt_7702_contract,
        default_chain_contract_settings(1)
            .expect("ethereum preset")
            .relay_adapt_7702_contract
    );
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
    assert!(settings.tokens.price_anchors.is_empty());
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

    assert!(settings.tokens.price_anchors.is_empty());
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
