use super::{
    DEFAULT_AUTO_LOCK_TIMEOUT_SECS, DbStore, OFFICIAL_INDEXED_ARTIFACT_GATEWAYS,
    OFFICIAL_POI_ARTIFACT_GATEWAYS, RememberedWalletKind, Url, WALLET_SETTINGS_KEY,
    WALLET_SETTINGS_VERSION, WALLET_UI_STATE_KEY, WALLET_UI_STATE_VERSION, WalletSettings,
    WalletSettingsError, WalletUiState, WalletUiStateError,
};

// All application settings writers share this lock, including startup repair and migration.
// DbStore commits each record atomically; this also serializes revision comparison with writing.
static SETTINGS_WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Frozen previous POI defaults used to identify released settings that have not been customized.
const PREVIOUS_OFFICIAL_POI_ARTIFACT_GATEWAYS: &[&str] = &[
    "https://dweb.link",
    "https://ipfs.filebase.io",
    "https://ipfs.io",
];

/// Loads and migrates a supported settings record without requiring semantic validity.
/// Runtime consumers must validate the returned settings before using them.
pub fn load_wallet_settings(store: &DbStore) -> Result<WalletSettings, WalletSettingsError> {
    let _writer = SETTINGS_WRITER
        .lock()
        .map_err(|_| WalletSettingsError::WriteUnavailable)?;
    load_wallet_settings_locked(store)
}

fn load_wallet_settings_locked(store: &DbStore) -> Result<WalletSettings, WalletSettingsError> {
    load_wallet_settings_with_writer(store, |payload| {
        store
            .put_app_settings_record(WALLET_SETTINGS_KEY, payload)
            .map_err(Into::into)
    })
}

pub(super) fn load_wallet_settings_with_writer(
    store: &DbStore,
    write: impl FnOnce(&[u8]) -> Result<(), WalletSettingsError>,
) -> Result<WalletSettings, WalletSettingsError> {
    let Some(payload) = store.get_app_settings_record(WALLET_SETTINGS_KEY)? else {
        return Ok(WalletSettings::default());
    };
    let (mut settings, version_migrated) = decode_wallet_settings_with_migration(&payload)?;
    let identity_migrated = settings.poi.artifact.migrate_legacy_official_identity();
    let indexed_gateway_migrated = version_migrated.is_some_and(|version| version < 6)
        && migrate_legacy_indexed_artifact_gateways(&mut settings);
    let poi_gateway_migrated = version_migrated.is_some_and(|version| version < 6)
        && migrate_previous_official_poi_gateways(&mut settings);
    if version_migrated.is_some()
        || identity_migrated
        || poi_gateway_migrated
        || indexed_gateway_migrated
    {
        let payload = encode_wallet_settings(&settings)?;
        write(&payload)?;
    }
    Ok(settings)
}

fn migrate_previous_official_poi_gateways(settings: &mut WalletSettings) -> bool {
    let previous_official_gateways: Vec<String> = PREVIOUS_OFFICIAL_POI_ARTIFACT_GATEWAYS
        .iter()
        .map(ToString::to_string)
        .collect();
    if settings.poi.artifact.gateway_urls != previous_official_gateways {
        return false;
    }

    settings.poi.artifact.gateway_urls = OFFICIAL_POI_ARTIFACT_GATEWAYS
        .iter()
        .map(ToString::to_string)
        .collect();
    true
}

fn migrate_legacy_indexed_artifact_gateways(settings: &mut WalletSettings) -> bool {
    let previous_official_poi_gateways: Vec<String> = PREVIOUS_OFFICIAL_POI_ARTIFACT_GATEWAYS
        .iter()
        .map(ToString::to_string)
        .collect();
    if settings.poi.artifact.gateway_urls != previous_official_poi_gateways {
        return false;
    }

    let official_indexed_gateways: Vec<String> = OFFICIAL_INDEXED_ARTIFACT_GATEWAYS
        .iter()
        .map(ToString::to_string)
        .collect();
    let legacy_gateways = &settings.indexed_artifacts.gateway_urls;
    if legacy_gateways == &official_indexed_gateways
        || legacy_gateways.is_empty()
        || legacy_gateways
            .iter()
            .any(|gateway| match Url::parse(gateway) {
                Ok(url) => !matches!(url.scheme(), "http" | "https"),
                Err(_) => true,
            })
    {
        return false;
    }

    settings.poi.artifact.gateway_urls = legacy_gateways.clone();
    true
}

pub fn save_wallet_settings(
    store: &DbStore,
    settings: &WalletSettings,
) -> Result<(), WalletSettingsError> {
    let _writer = SETTINGS_WRITER
        .lock()
        .map_err(|_| WalletSettingsError::WriteUnavailable)?;
    save_wallet_settings_locked(store, settings)
}

/// Commits a whole or targeted draft only if its saved base is still current.
pub fn commit_wallet_settings(
    store: &DbStore,
    expected: super::SettingsRevision,
    settings: &WalletSettings,
) -> Result<(), WalletSettingsError> {
    let _writer = SETTINGS_WRITER
        .lock()
        .map_err(|_| WalletSettingsError::WriteUnavailable)?;
    let current = load_wallet_settings_locked(store)?;
    if super::settings_revision(&current)? != expected {
        return Err(WalletSettingsError::Conflict);
    }
    save_wallet_settings_locked(store, settings)
}

fn save_wallet_settings_locked(
    store: &DbStore,
    settings: &WalletSettings,
) -> Result<(), WalletSettingsError> {
    let mut settings = settings.clone();
    settings.version = WALLET_SETTINGS_VERSION;
    settings.validate()?;
    let payload = encode_wallet_settings(&settings)?;
    store.put_app_settings_record(WALLET_SETTINGS_KEY, &payload)?;
    Ok(())
}

pub fn delete_wallet_settings(store: &DbStore) -> Result<(), WalletSettingsError> {
    let _writer = SETTINGS_WRITER
        .lock()
        .map_err(|_| WalletSettingsError::WriteUnavailable)?;
    store.delete_app_settings_record(WALLET_SETTINGS_KEY)?;
    Ok(())
}

pub fn load_wallet_ui_state(store: &DbStore) -> Result<WalletUiState, WalletUiStateError> {
    let Some(payload) = store.get_app_settings_record(WALLET_UI_STATE_KEY)? else {
        return Ok(WalletUiState::default());
    };

    match decode_wallet_ui_state_with_migration(&payload) {
        Ok((state, migrated)) => {
            if migrated {
                let payload = encode_wallet_ui_state(&state)?;
                store.put_app_settings_record(WALLET_UI_STATE_KEY, &payload)?;
            }
            Ok(state)
        }
        Err(
            error @ (WalletUiStateError::Decode(_) | WalletUiStateError::UnsupportedVersion { .. }),
        ) => {
            tracing::warn!(%error, "ignoring invalid wallet UI state");
            Ok(WalletUiState::default())
        }
        Err(error) => Err(error),
    }
}

pub fn save_wallet_ui_state(
    store: &DbStore,
    state: &WalletUiState,
) -> Result<(), WalletUiStateError> {
    let payload = encode_wallet_ui_state(state)?;
    store.put_app_settings_record(WALLET_UI_STATE_KEY, &payload)?;
    Ok(())
}

pub fn encode_wallet_settings(settings: &WalletSettings) -> Result<Vec<u8>, WalletSettingsError> {
    let mut settings = settings.clone();
    settings.version = WALLET_SETTINGS_VERSION;
    Ok(rmp_serde::to_vec_named(&settings)?)
}

pub fn decode_wallet_settings(data: &[u8]) -> Result<WalletSettings, WalletSettingsError> {
    decode_wallet_settings_with_migration(data).map(|(settings, _migrated)| settings)
}

fn decode_wallet_settings_with_migration(
    data: &[u8],
) -> Result<(WalletSettings, Option<u32>), WalletSettingsError> {
    #[derive(serde::Deserialize)]
    struct Version {
        #[serde(default = "released_version")]
        version: u32,
    }
    const fn released_version() -> u32 {
        6
    }
    let version: Version = rmp_serde::from_slice(data)?;
    let (mut settings, migrated) = match version.version {
        WALLET_SETTINGS_VERSION => Ok((rmp_serde::from_slice(data)?, None)),
        7..=8 => {
            let mut settings: WalletSettings = rmp_serde::from_slice(data)?;
            settings.version = WALLET_SETTINGS_VERSION;
            Ok((settings, Some(version.version)))
        }
        1..=6 => {
            let mut legacy: super::legacy::LegacyWalletSettings = rmp_serde::from_slice(data)?;
            if version.version == 1 {
                legacy.runtime.auto_lock_timeout_secs = Some(DEFAULT_AUTO_LOCK_TIMEOUT_SECS);
            }
            if version.version == 3 {
                legacy.privacy.mimic_railway_shields_by_default = false;
            }
            Ok((legacy.into(), Some(version.version)))
        }
        version => Err(WalletSettingsError::UnsupportedVersion { version }),
    }?;
    if migrated.is_some() {
        settings.waku.backup_peers = settings.waku.direct_peers.as_ref().map(|_| Vec::new());
        // Freeze the released default so future endpoint changes cannot change this migration.
        if let Some([peer]) = settings.waku.direct_peers.as_deref()
            && peer.peer_id == "16Uiu2HAkwhijhoc4UxAJD4fmYgSX91FzSDqehAaxJogYFcyo736a"
            && peer.addr == "/dns4/baaamooobaaa.mooo.com/tcp/8000/wss"
        {
            settings.waku.backup_peers = Some(vec![peer.clone()]);
            settings.waku.direct_peers = None;
        }
    }
    Ok((settings, migrated))
}

pub fn encode_wallet_ui_state(state: &WalletUiState) -> Result<Vec<u8>, WalletUiStateError> {
    let mut state = state.clone();
    state.version = WALLET_UI_STATE_VERSION;
    Ok(rmp_serde::to_vec_named(&state)?)
}

pub fn decode_wallet_ui_state(data: &[u8]) -> Result<WalletUiState, WalletUiStateError> {
    decode_wallet_ui_state_with_migration(data).map(|(state, _migrated)| state)
}

fn decode_wallet_ui_state_with_migration(
    data: &[u8],
) -> Result<(WalletUiState, bool), WalletUiStateError> {
    let mut state: WalletUiState = rmp_serde::from_slice(data)?;
    let migrated = match state.version {
        WALLET_UI_STATE_VERSION => false,
        1 => {
            state.version = WALLET_UI_STATE_VERSION;
            state.last_wallet_kind = RememberedWalletKind::default();
            state.governance_participants.clear();
            state.last_public_accounts.clear();
            true
        }
        2 => {
            state.version = WALLET_UI_STATE_VERSION;
            state.governance_participants.clear();
            state.last_public_accounts.clear();
            true
        }
        3 => {
            state.version = WALLET_UI_STATE_VERSION;
            state.last_public_accounts.clear();
            true
        }
        version => {
            return Err(WalletUiStateError::UnsupportedVersion { version });
        }
    };
    Ok((state, migrated))
}
