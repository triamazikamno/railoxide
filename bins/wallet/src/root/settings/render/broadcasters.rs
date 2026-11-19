use super::*;

impl WalletSettingsEditor {
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    pub(super) fn broadcasters_page(&self, editor: &Entity<Self>) -> SettingPage {
        let positive_number_options = NumberFieldOptions {
            min: 1.0,
            max: 86_400.0,
            step: 1.0,
        };
        let waku_max_peers = Self::number_field(
            "waku-max-peers",
            editor.clone(),
            positive_number_options.clone(),
            None,
            |settings| settings.waku.max_peers as f64,
            |settings, value| settings.waku.max_peers = value as usize,
        );
        let waku_timeout = Self::number_field(
            "waku-peer-timeout-seconds",
            editor.clone(),
            positive_number_options.clone(),
            Some("s"),
            |settings| settings.waku.peer_connection_timeout_secs as f64,
            |settings, value| settings.waku.peer_connection_timeout_secs = value as u64,
        );
        let broadcaster_timeout = Self::number_field(
            "broadcaster-response-timeout-seconds",
            editor.clone(),
            positive_number_options.clone(),
            Some("s"),
            |settings| settings.broadcaster.response_timeout_secs as f64,
            |settings, value| settings.broadcaster.response_timeout_secs = value as u64,
        );
        let broadcaster_republish_interval = Self::number_field(
            "broadcaster-republish-interval-seconds",
            editor.clone(),
            positive_number_options,
            Some("s"),
            |settings| settings.broadcaster.republish_interval_secs as f64,
            |settings, value| settings.broadcaster.republish_interval_secs = value as u64,
        );
        let waku_doh = Self::shared_string_field(
            "waku-doh-endpoint",
            editor.clone(),
            display_waku_doh_endpoint,
            |settings, value| settings.waku.doh_endpoint = non_empty_setting(&value),
        );
        let waku_nwaku = Self::shared_string_field(
            "waku-nwaku-rest-url",
            editor.clone(),
            |settings| settings.waku.nwaku_url.clone().unwrap_or_default(),
            |settings, value| settings.waku.nwaku_url = non_empty_setting(&value),
        );
        let waku_dns_enr_kind = SettingsUrlListKind::WakuDnsEnrTree;
        let waku_dns_enr_trees = waku_dns_enr_kind.endpoints(&self.draft);
        let waku_dns_enr_editor = editor.clone();
        let waku_direct_peers = WakuPeerList::Direct.peers(&self.draft);
        let waku_direct_peers_editor = editor.clone();
        let waku_backup_peers = WakuPeerList::Backup.peers(&self.draft);
        let waku_backup_peers_editor = editor.clone();
        let waku_doh_fallback_kind = SettingsUrlListKind::WakuDohFallback;
        let waku_doh_fallback_endpoints = waku_doh_fallback_kind.endpoints(&self.draft);
        let waku_doh_fallback_editor = editor.clone();

        SettingPage::new("Broadcasters")
            .group(
                settings_group()
                    .item(
                        Self::broadcaster_anchor_range_item(editor.clone()).keywords([
                            "broadcaster",
                            "relayer",
                            "fees",
                            "suspicious",
                            "premium",
                            "percentage",
                        ]),
                    )
                    .item(
                        Self::settings_switch_item(
                            "wallet-settings-broadcaster-allow-suspicious",
                            "Allow suspicious by default",
                            editor.clone(),
                            None,
                            |settings| {
                                settings
                                    .broadcaster
                                    .allow_suspicious_broadcasters_by_default
                            },
                            |settings, value| {
                                settings
                                    .broadcaster
                                    .allow_suspicious_broadcasters_by_default = value;
                            },
                        )
                        .keywords([
                            "broadcaster",
                            "relayer",
                            "fees",
                            "accepted fee range",
                        ]),
                    ),
            )
            .group(
                settings_group()
                    .title("Advanced")
                    .item(
                        SettingItem::new("Response timeout", broadcaster_timeout).keywords([
                            "broadcaster",
                            "relayer",
                            "seconds",
                            "advanced",
                        ]),
                    )
                    .item(
                        SettingItem::new("Republish interval", broadcaster_republish_interval)
                            .keywords([
                                "broadcaster",
                                "relayer",
                                "resend",
                                "retry",
                                "seconds",
                                "advanced",
                            ]),
                    ),
            )
            .group(
                settings_group()
                    .title("Waku connectivity")
                    .item(
                        Self::settings_url_list_item(
                            "DNS ENR trees",
                            waku_dns_enr_editor,
                            waku_dns_enr_kind,
                            waku_dns_enr_trees,
                        )
                        .keywords([
                            "waku",
                            "waku connectivity",
                            "discovery",
                            "bootstrap",
                            "enrtree",
                            "peers",
                        ]),
                    )
                    .item(
                        Self::waku_peer_list_item(
                            waku_direct_peers_editor,
                            WakuPeerList::Direct,
                            waku_direct_peers,
                        )
                        .keywords([
                            "waku",
                            "waku connectivity",
                            "multiaddr",
                            "peer id",
                            "bootstrap",
                        ]),
                    )
                    .item(
                        Self::waku_peer_list_item(
                            waku_backup_peers_editor,
                            WakuPeerList::Backup,
                            waku_backup_peers,
                        )
                        .keywords([
                            "waku",
                            "waku connectivity",
                            "multiaddr",
                            "peer id",
                            "fallback",
                        ]),
                    )
                    .item(
                        SettingItem::new("DoH endpoint", waku_doh)
                            .keywords(["waku", "waku connectivity", "dns over https", "resolver"])
                            .layout(Axis::Vertical),
                    )
                    .item(
                        Self::settings_url_list_item(
                            "DoH fallback endpoints",
                            waku_doh_fallback_editor,
                            waku_doh_fallback_kind,
                            waku_doh_fallback_endpoints,
                        )
                        .keywords([
                            "waku",
                            "waku connectivity",
                            "dns over https",
                            "resolver",
                        ]),
                    )
                    .item(SettingItem::new("Max peers", waku_max_peers).keywords([
                        "waku",
                        "waku connectivity",
                        "connections",
                        "limit",
                    ]))
                    .item(SettingItem::new("Peer timeout", waku_timeout).keywords([
                        "waku",
                        "waku connectivity",
                        "seconds",
                        "connection",
                    ]))
                    .item(
                        SettingItem::new("nwaku REST URL", waku_nwaku)
                            .keywords(["waku", "waku connectivity", "node", "rest api", "endpoint"])
                            .layout(Axis::Vertical),
                    ),
            )
    }
}
