use super::*;

#[test]
fn waku_settings_defaults_match_startup_defaults() {
    let settings = WalletSettings::default();
    assert_eq!(
        settings.waku.cluster_id,
        crate::settings::DEFAULT_WAKU_CLUSTER_ID
    );
    assert_eq!(
        settings.waku.shard_id,
        crate::settings::DEFAULT_WAKU_SHARD_ID
    );
    assert!(settings.waku.dns_enr_trees.is_none());
    assert!(settings.waku.direct_peers.is_none());
    assert!(settings.waku.doh_endpoint.is_none());
    assert!(settings.waku.doh_fallback_endpoints.is_none());
    assert_eq!(
        settings.waku.max_peers,
        crate::settings::DEFAULT_WAKU_MAX_PEERS
    );
    assert_eq!(
        settings.waku.peer_connection_timeout_secs,
        crate::settings::DEFAULT_WAKU_PEER_CONNECTION_TIMEOUT_SECS
    );
}

#[test]
fn waku_dns_enr_trees_validate_scheme() {
    let mut settings = WalletSettings::default();
    settings.waku.dns_enr_trees = Some(vec!["https://bad.example".to_string()]);

    let err = settings.validate().expect_err("bad DNS ENR tree rejected");

    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("waku.dns_enr_trees[0]"))
    );
}

#[test]
fn default_waku_backup_peer_is_valid() {
    let peers = crate::settings::default_waku_backup_peers();
    assert_eq!(peers.len(), 1);
    assert_eq!(
        peers[0].peer_id,
        crate::settings::DEFAULT_WAKU_BACKUP_PEER_ID
    );
    assert_eq!(
        peers[0].addr,
        crate::settings::DEFAULT_WAKU_BACKUP_PEER_ADDR
    );

    let mut settings = WalletSettings::default();
    settings.waku.backup_peers = Some(peers);
    settings.validate().expect("default backup peer is valid");
}

#[test]
fn waku_peers_validate_peer_id_and_multiaddr() {
    for field in ["direct_peers", "backup_peers"] {
        let mut settings = WalletSettings::default();
        let peers = Some(vec![WakuDirectPeerSetting {
            peer_id: "not-a-peer-id".to_string(),
            addr: "not-a-multiaddr".to_string(),
        }]);
        if field == "direct_peers" {
            settings.waku.direct_peers = peers;
        } else {
            settings.waku.backup_peers = peers;
        }
        let err = settings.validate().expect_err("bad peer rejected");
        for member in ["peer_id", "addr"] {
            assert!(
                err.messages
                    .iter()
                    .any(|message| message.contains(&format!("waku.{field}[0].{member}")))
            );
        }
    }
}

#[test]
fn waku_doh_fallback_endpoints_validate_url_schemes() {
    let mut settings = WalletSettings::default();
    settings.waku.doh_fallback_endpoints = Some(vec!["ftp://bad.example/dns-query".to_string()]);

    let err = settings
        .validate()
        .expect_err("bad DoH fallback scheme rejected");

    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("waku.doh_fallback_endpoints[0]"))
    );
}
