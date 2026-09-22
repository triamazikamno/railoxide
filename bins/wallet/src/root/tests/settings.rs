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
        vec!["https://rpc.example".into()];
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
    assert!(WakuPeerList::Backup.peers(&settings).is_empty());
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

#[gpui::test]
fn shared_chain_editor_commits_discards_resets_and_preserves_stale_settings(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::{
        AppContext as _, IntoElement, ParentElement as _, Render, Styled as _, VisualTestContext,
        div,
    };
    use railgun_ui::chain_editor::{ChainDraft, ChainEditorCommand, ChainField};
    use std::{cell::RefCell, rc::Rc};

    /// The app root paints the dialog layer; the bare editor does not, so confirmations need a host.
    struct ChainEditorHost(Entity<ui::chain_editor::ChainEditor>);

    impl Render for ChainEditorHost {
        fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(self.0.clone())
                .children(gpui_component::Root::render_dialog_layer(window, cx))
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
    click(cx, "edit-chain-1");
    click(cx, "chain-enabled");
    click(cx, "chain-save");
    saved.chains.per_chain.get_mut(&1).unwrap().enabled = false;
    assert_eq!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap(),
        saved
    );
    // Saving returns to the list; reopen the chain before resetting it.
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
    saved.chains.per_chain.remove(&1);
    assert_eq!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref()).unwrap(),
        saved
    );

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
            editor.handle_chain_editor_probe(&draft, cx);
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
                (
                    ChainField::RpcEndpoints,
                    "https://synthetic:credential@rpc.example",
                ),
            ] {
                draft.fields.insert(field, value.into());
            }
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
        });
    });
    assert!(
        wallet_ops::settings::load_wallet_settings(store.db().as_ref())
            .unwrap()
            .chains
            .custom
            .contains_key(&9_007_199_254_740_993)
    );
    // Leaving the reset draft returns the shared list, which already carries that addition.
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
