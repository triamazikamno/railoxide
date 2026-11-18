use super::*;

#[test]
fn validation_rejects_ambiguous_proxy_and_disabled_chains() {
    let mut settings = WalletSettings::default();
    settings.network.proxy_url = Some("http://127.0.0.1:9050".to_string());
    for chain in settings.chains.per_chain.values_mut() {
        chain.enabled = false;
    }

    let err = settings.validate().expect_err("settings invalid");
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("proxy_url"))
    );
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("at least one supported chain"))
    );
}

#[test]
fn auto_lock_settings_default_disabled_and_bounds_are_validated() {
    let mut settings = WalletSettings::default();
    assert_eq!(
        settings.runtime.auto_lock_timeout_secs,
        Some(DEFAULT_AUTO_LOCK_TIMEOUT_SECS)
    );
    assert!(
        AUTO_LOCK_TIMEOUT_PRESETS_SECS.contains(&DEFAULT_AUTO_LOCK_TIMEOUT_SECS),
        "the default must be exposed as a Settings preset"
    );

    settings.runtime.auto_lock_timeout_secs = None;
    settings.validate().expect("Disabled is valid");

    for timeout in [MIN_AUTO_LOCK_TIMEOUT_SECS, MAX_AUTO_LOCK_TIMEOUT_SECS] {
        settings.runtime.auto_lock_timeout_secs = Some(timeout);
        settings.validate().expect("timeout bound is valid");
    }

    for timeout in [
        MIN_AUTO_LOCK_TIMEOUT_SECS - 1,
        MAX_AUTO_LOCK_TIMEOUT_SECS + 1,
    ] {
        settings.runtime.auto_lock_timeout_secs = Some(timeout);
        let error = settings
            .validate()
            .expect_err("out-of-range timeout rejected");
        assert!(
            error
                .messages
                .iter()
                .any(|message| message.contains("runtime.auto_lock_timeout_secs"))
        );
    }
}

#[test]
fn reset_helpers_restore_defaults() {
    let mut settings = WalletSettings::default();
    settings.network.mode = crate::settings::NetworkModeSetting::Direct;
    settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
    settings.poi.artifact.gateway_urls.clear();
    settings.walletconnect.project_id_override = Some("custom-project".to_owned());

    settings.reset_network();
    settings.reset_indexed_artifacts();
    settings.reset_poi();
    settings.reset_walletconnect();

    assert_eq!(
        settings.network,
        crate::settings::NetworkSettings::default()
    );
    assert_eq!(
        settings.indexed_artifacts,
        crate::settings::IndexedArtifactSettings::default()
    );
    assert_eq!(settings.poi, crate::settings::PoiSettings::default());
    assert_eq!(
        settings.walletconnect,
        crate::settings::WalletConnectSettings::default()
    );
}

#[test]
fn walletconnect_settings_use_default_or_project_id_override() {
    let mut settings = WalletSettings::default();

    assert_eq!(
        settings.walletconnect.effective_project_id(),
        WALLETCONNECT_DEFAULT_PROJECT_ID
    );

    settings.walletconnect.project_id_override = Some("user-project-id".to_owned());

    assert_eq!(
        settings.walletconnect.effective_project_id(),
        "user-project-id"
    );
}

#[test]
fn walletconnect_settings_reject_empty_project_id_override() {
    let mut settings = WalletSettings::default();
    settings.walletconnect.project_id_override = Some("   ".to_owned());

    let err = settings.validate().expect_err("empty override rejected");
    assert!(err.messages.iter().any(|message| {
        message.contains("walletconnect.project_id_override")
            && message.contains("must not be empty")
    }));
}

#[test]
fn walletconnect_settings_do_not_persist_custom_relay_url() {
    let mut settings = WalletSettings::default();
    settings.walletconnect.project_id_override = Some("user-project-id".to_owned());

    let encoded = encode_wallet_settings(&settings).expect("encode settings");
    let serialized = serde_json::to_value(&settings).expect("serialize settings");

    assert_eq!(
        serialized["walletconnect"]["project_id_override"],
        "user-project-id"
    );
    assert!(serialized["walletconnect"].get("relay_url").is_none());
    assert!(!String::from_utf8_lossy(&encoded).contains("relay_url"));
}
