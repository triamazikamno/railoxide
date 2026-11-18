use super::*;

#[test]
fn stored_invalid_settings_load_for_repair_but_cannot_be_saved() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut settings = WalletSettings::default();
    settings.runtime.auto_lock_timeout_secs = Some(MIN_AUTO_LOCK_TIMEOUT_SECS - 1);
    settings.walletconnect.project_id_override = Some("preserved-project-id".to_string());
    let payload = rmp_serde::to_vec_named(&settings).expect("encode invalid settings fixture");
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &payload)
        .expect("store invalid settings fixture");

    let loaded = load_wallet_settings(&store).expect("load invalid settings for repair");
    assert_eq!(loaded, settings);
    assert_eq!(
        loaded.walletconnect.project_id_override.as_deref(),
        Some("preserved-project-id")
    );
    assert!(loaded.validate().is_err());
    let error =
        save_wallet_settings(&store, &loaded).expect_err("invalid settings rejected on save");
    assert!(matches!(error, WalletSettingsError::Validation(_)));
    assert_eq!(
        store
            .get_app_settings_record(WALLET_SETTINGS_KEY)
            .expect("reread invalid settings")
            .expect("invalid settings record"),
        payload,
        "loading and rejected saving must not rewrite a current-version record"
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn first_start_record_loads_incomplete_until_saved_and_survives_settings_saves() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");

    assert_eq!(
        load_first_start(&store).expect("load missing record"),
        WalletFirstStart::default()
    );
    assert!(
        store
            .get_app_settings_record(WALLET_FIRST_START_KEY)
            .expect("read raw record")
            .is_none()
    );

    store
        .put_app_settings_record(WALLET_FIRST_START_KEY, b"not a record")
        .expect("store undecodable record");
    let undecodable = load_first_start(&store).expect("load undecodable record");
    assert!(!undecodable.setup_complete);
    assert_eq!(undecodable.accepted_notice_version, 0);

    let record = WalletFirstStart {
        version: 0,
        setup_complete: true,
        accepted_notice_version: 3,
    };
    save_first_start(&store, &record).expect("save record");
    let saved = store
        .get_app_settings_record(WALLET_FIRST_START_KEY)
        .expect("read saved record")
        .expect("saved record");
    save_wallet_settings(&store, &WalletSettings::default()).expect("save default settings");
    assert_eq!(
        store
            .get_app_settings_record(WALLET_FIRST_START_KEY)
            .expect("read record after settings save"),
        Some(saved)
    );
    assert_eq!(
        load_first_start(&store).expect("load saved record"),
        WalletFirstStart {
            version: WALLET_FIRST_START_VERSION,
            ..record
        }
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn missing_ui_state_defaults_to_empty_without_persisting() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");

    let state = load_wallet_ui_state(&store).expect("load UI state");
    assert_eq!(state, WalletUiState::default());
    assert!(
        store
            .get_app_settings_record(WALLET_UI_STATE_KEY)
            .expect("load raw UI state")
            .is_none()
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn ui_state_roundtrip_through_local_db() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let last_public_accounts = std::collections::BTreeMap::from([
        ("wallet-123".to_owned(), "account-2".to_owned()),
        ("hardware-123".to_owned(), "hardware-account".to_owned()),
    ]);
    let excluded = std::collections::BTreeSet::from(["account-excluded".to_owned()]);
    let state = WalletUiState {
        version: 0,
        public_account_sort: crate::settings::PublicAccountSort::Added,
        public_hide_empty_accounts: true,
        swap_disclaimer_acknowledged: true,
        public_auto_refresh: true,
        public_refresh_excluded: excluded.clone(),
        last_wallet_id: Some("wallet-123".to_owned()),
        last_chain_id: Some(137),
        last_wallet_kind: RememberedWalletKind::SoftwareProfile,
        last_public_accounts: last_public_accounts.clone(),
        governance_participants: std::collections::BTreeMap::from([(
            "wallet-123".to_owned(),
            vec!["account-1".to_owned(), "account-2".to_owned()],
        )]),
    };

    save_wallet_ui_state(&store, &state).expect("save UI state");
    // Exclusions are keyed by account id only; no account address reaches plaintext UI state.
    let excluded_address = Address::repeat_byte(0xa5);
    let stored = store
        .get_app_settings_record(WALLET_UI_STATE_KEY)
        .expect("read stored UI state")
        .expect("stored UI state");
    assert!(stored.windows(16).any(|w| w == b"account-excluded"));
    assert!(!stored.windows(20).any(|w| w == excluded_address.as_slice()));
    let address_hex = alloy::hex::encode(excluded_address);
    assert!(
        !stored
            .windows(address_hex.len())
            .any(|w| w == address_hex.as_bytes())
    );
    drop(store);
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("reopen db");
    let loaded = load_wallet_ui_state(&store).expect("load UI state after restart");

    assert_eq!(
        loaded,
        WalletUiState {
            version: WALLET_UI_STATE_VERSION,
            public_account_sort: crate::settings::PublicAccountSort::Added,
            public_hide_empty_accounts: true,
            swap_disclaimer_acknowledged: true,
            public_auto_refresh: true,
            public_refresh_excluded: excluded,
            last_wallet_id: Some("wallet-123".to_owned()),
            last_chain_id: Some(137),
            last_wallet_kind: RememberedWalletKind::SoftwareProfile,
            last_public_accounts,
            governance_participants: std::collections::BTreeMap::from([(
                "wallet-123".to_owned(),
                vec!["account-1".to_owned(), "account-2".to_owned()],
            )]),
        }
    );

    save_wallet_ui_state(
        &store,
        &WalletUiState {
            version: WALLET_UI_STATE_VERSION,
            last_wallet_id: Some("hardware-123".to_owned()),
            last_chain_id: Some(137),
            last_wallet_kind: RememberedWalletKind::HardwareWallet,
            governance_participants: std::collections::BTreeMap::new(),
            last_public_accounts: std::collections::BTreeMap::new(),
            ..WalletUiState::default()
        },
    )
    .expect("save hardware UI state");
    assert_eq!(
        load_wallet_ui_state(&store).expect("load hardware UI state"),
        WalletUiState {
            version: WALLET_UI_STATE_VERSION,
            last_wallet_id: Some("hardware-123".to_owned()),
            last_chain_id: Some(137),
            last_wallet_kind: RememberedWalletKind::HardwareWallet,
            governance_participants: std::collections::BTreeMap::new(),
            last_public_accounts: std::collections::BTreeMap::new(),
            ..WalletUiState::default()
        }
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn released_v1_ui_state_migrates_to_unknown_kind_and_persists_current() {
    #[derive(Serialize)]
    struct ReleasedV1WalletUiState {
        version: u32,
        last_wallet_id: Option<String>,
        last_chain_id: Option<u64>,
    }

    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let payload = rmp_serde::to_vec_named(&ReleasedV1WalletUiState {
        version: 1,
        last_wallet_id: Some("legacy-wallet".to_owned()),
        last_chain_id: Some(56),
    })
    .expect("encode released v1 UI state");
    store
        .put_app_settings_record(WALLET_UI_STATE_KEY, &payload)
        .expect("store released v1 UI state");

    let loaded = load_wallet_ui_state(&store).expect("migrate UI state");
    assert_eq!(loaded.version, WALLET_UI_STATE_VERSION);
    assert_eq!(loaded.last_wallet_id.as_deref(), Some("legacy-wallet"));
    assert_eq!(loaded.last_chain_id, Some(56));
    assert_eq!(loaded.last_wallet_kind, RememberedWalletKind::Unknown);
    assert!(loaded.governance_participants.is_empty());
    let persisted = store
        .get_app_settings_record(WALLET_UI_STATE_KEY)
        .expect("load migrated UI state")
        .expect("migrated UI state");
    assert_eq!(
        decode_wallet_ui_state(&persisted).expect("decode migrated UI state"),
        loaded
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn released_v2_ui_state_migrates_with_empty_participants() {
    #[derive(Serialize)]
    struct ReleasedV2WalletUiState {
        version: u32,
        last_wallet_id: Option<String>,
        last_chain_id: Option<u64>,
        last_wallet_kind: RememberedWalletKind,
    }

    let payload = rmp_serde::to_vec_named(&ReleasedV2WalletUiState {
        version: 2,
        last_wallet_id: Some("v2-wallet".to_owned()),
        last_chain_id: Some(1),
        last_wallet_kind: RememberedWalletKind::HardwareWallet,
    })
    .expect("encode released v2 UI state");
    let loaded = decode_wallet_ui_state(&payload).expect("migrate released v2 UI state");
    assert_eq!(loaded.version, WALLET_UI_STATE_VERSION);
    assert_eq!(loaded.last_wallet_id.as_deref(), Some("v2-wallet"));
    assert_eq!(
        loaded.last_wallet_kind,
        RememberedWalletKind::HardwareWallet
    );
    assert!(loaded.governance_participants.is_empty());
}

#[test]
fn released_v3_ui_state_preserves_preferences_without_an_account_selection() {
    #[derive(Serialize)]
    struct ReleasedV3WalletUiState {
        version: u32,
        last_wallet_id: Option<String>,
        last_chain_id: Option<u64>,
        last_wallet_kind: RememberedWalletKind,
        governance_participants: std::collections::BTreeMap<String, Vec<String>>,
    }

    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let previous = ReleasedV3WalletUiState {
        version: 3,
        last_wallet_id: Some("wallet-123".to_owned()),
        last_chain_id: Some(137),
        last_wallet_kind: RememberedWalletKind::SoftwareProfile,
        governance_participants: std::collections::BTreeMap::from([(
            "wallet-123".to_owned(),
            vec!["participant".to_owned()],
        )]),
    };
    store
        .put_app_settings_record(
            WALLET_UI_STATE_KEY,
            &rmp_serde::to_vec_named(&previous).expect("encode released v3 UI state"),
        )
        .expect("store previous state");

    let loaded = load_wallet_ui_state(&store).expect("migrate v3 UI state");
    assert_eq!(loaded.last_wallet_id, previous.last_wallet_id);
    assert_eq!(loaded.last_chain_id, previous.last_chain_id);
    assert_eq!(loaded.last_wallet_kind, previous.last_wallet_kind);
    assert_eq!(
        loaded.governance_participants,
        previous.governance_participants
    );
    assert!(loaded.last_public_accounts.is_empty());
    let persisted = store
        .get_app_settings_record(WALLET_UI_STATE_KEY)
        .expect("read migration")
        .expect("persisted migration");
    let persisted: WalletUiState =
        rmp_serde::from_slice(&persisted).expect("decode without migrating");
    assert_eq!(persisted.version, WALLET_UI_STATE_VERSION);
    assert_eq!(persisted, loaded);
    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn unsupported_future_ui_state_version_falls_back_to_empty() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let state = WalletUiState {
        version: WALLET_UI_STATE_VERSION + 1,
        last_wallet_id: Some("wallet-123".to_owned()),
        last_chain_id: Some(137),
        last_wallet_kind: RememberedWalletKind::Unknown,
        governance_participants: std::collections::BTreeMap::new(),
        last_public_accounts: std::collections::BTreeMap::new(),
        ..WalletUiState::default()
    };
    let data = rmp_serde::to_vec_named(&state).expect("encode future UI state");
    store
        .put_app_settings_record(WALLET_UI_STATE_KEY, &data)
        .expect("store future UI state");

    let err = decode_wallet_ui_state(&data).expect_err("future version rejected");
    assert!(matches!(
        err,
        WalletUiStateError::UnsupportedVersion { version }
            if version == WALLET_UI_STATE_VERSION + 1
    ));
    let loaded = load_wallet_ui_state(&store).expect("load UI state");
    assert_eq!(loaded, WalletUiState::default());

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn corrupt_ui_state_falls_back_to_empty() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    store
        .put_app_settings_record(WALLET_UI_STATE_KEY, &[0xc1])
        .expect("store corrupt UI state");

    let loaded = load_wallet_ui_state(&store).expect("load UI state");
    assert_eq!(loaded, WalletUiState::default());

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn unsupported_future_settings_version_is_rejected() {
    let settings = WalletSettings {
        version: WALLET_SETTINGS_VERSION + 1,
        ..WalletSettings::default()
    };
    let data = rmp_serde::to_vec_named(&settings).expect("encode future settings");

    let err = decode_wallet_settings(&data).expect_err("future version rejected");
    assert!(matches!(
        err,
        WalletSettingsError::UnsupportedVersion { version }
            if version == WALLET_SETTINGS_VERSION + 1
    ));
}

#[test]
fn encoded_settings_decode_without_db() {
    let settings = WalletSettings::default();
    let data = encode_wallet_settings(&settings).expect("encode settings");
    let decoded = decode_wallet_settings(&data).expect("decode settings");
    assert_eq!(decoded, settings);
}

#[test]
fn released_v4_ui_state_preserves_account_memory_and_defaults_public_preferences() {
    let payload = rmp_serde::to_vec_named(&serde_json::json!({
        "version": 4,
        "last_wallet_id": "wallet-123",
        "last_public_accounts": {"wallet-123": "account-1"},
        "governance_participants": {"wallet-123": ["account-1"]}
    }))
    .unwrap();
    let state = decode_wallet_ui_state(&payload).unwrap();
    assert_eq!(state.version, WALLET_UI_STATE_VERSION);
    assert_eq!(
        state.public_account_sort,
        crate::settings::PublicAccountSort::Value
    );
    assert!(!state.public_hide_empty_accounts);
    assert!(!state.public_auto_refresh);
    assert!(state.public_refresh_excluded.is_empty());
    assert_eq!(state.last_public_accounts["wallet-123"], "account-1");
    assert_eq!(state.governance_participants["wallet-123"], ["account-1"]);
}
