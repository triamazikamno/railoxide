use super::*;

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
        crate::root::tests::StartupSettingsActionState {
            settings: true,
            reset: true,
            retry: true,
            maintenance_actions_enabled: true,
        }
    );
    assert_eq!(
        startup_settings_action_state(false, true),
        crate::root::tests::StartupSettingsActionState {
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
