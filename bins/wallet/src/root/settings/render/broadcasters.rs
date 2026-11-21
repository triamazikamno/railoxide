use super::*;

const POSITIVE_NUMBER_OPTIONS: NumberFieldOptions = NumberFieldOptions {
    min: 1.0,
    max: 86_400.0,
    step: 1.0,
};

impl WalletSettingsEditor {
    pub(super) fn broadcasters_page(&self, editor: &Entity<Self>) -> SettingPage {
        let fee_policy_group = Self::broadcaster_fee_policy_group(editor);
        let advanced_group = Self::broadcaster_advanced_group(editor);
        let waku_connectivity_group = self.waku_connectivity_group(editor);
        SettingPage::new("Broadcasters")
            .group(fee_policy_group)
            .group(advanced_group)
            .group(waku_connectivity_group)
    }

    fn broadcaster_fee_policy_group(editor: &Entity<Self>) -> SettingGroup {
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
                .keywords(["broadcaster", "relayer", "fees", "accepted fee range"]),
            )
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    fn broadcaster_advanced_group(editor: &Entity<Self>) -> SettingGroup {
        let broadcaster_timeout = Self::number_field(
            "broadcaster-response-timeout-seconds",
            editor.clone(),
            POSITIVE_NUMBER_OPTIONS,
            Some("s"),
            |settings| settings.broadcaster.response_timeout_secs as f64,
            |settings, value| settings.broadcaster.response_timeout_secs = value as u64,
        );
        let broadcaster_republish_interval = Self::number_field(
            "broadcaster-republish-interval-seconds",
            editor.clone(),
            POSITIVE_NUMBER_OPTIONS,
            Some("s"),
            |settings| settings.broadcaster.republish_interval_secs as f64,
            |settings, value| settings.broadcaster.republish_interval_secs = value as u64,
        );
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
                SettingItem::new("Republish interval", broadcaster_republish_interval).keywords([
                    "broadcaster",
                    "relayer",
                    "resend",
                    "retry",
                    "seconds",
                    "advanced",
                ]),
            )
    }

    fn waku_connectivity_group(&self, editor: &Entity<Self>) -> SettingGroup {
        settings_group()
            .title("Waku connectivity")
            .items(self.waku_discovery_items(editor))
            .items(self.waku_dns_resolution_items(editor))
            .items(Self::waku_connection_items(editor))
    }

    fn waku_discovery_items(&self, editor: &Entity<Self>) -> [SettingItem; 3] {
        let waku_dns_enr_kind = SettingsUrlListKind::WakuDnsEnrTree;
        let waku_dns_enr_trees = waku_dns_enr_kind.endpoints(&self.draft);
        let waku_direct_peers = WakuPeerList::Direct.peers(&self.draft);
        let waku_backup_peers = WakuPeerList::Backup.peers(&self.draft);
        [
            Self::settings_url_list_item(
                "DNS ENR trees",
                editor.clone(),
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
            Self::waku_peer_list_item(editor.clone(), WakuPeerList::Direct, waku_direct_peers)
                .keywords([
                    "waku",
                    "waku connectivity",
                    "multiaddr",
                    "peer id",
                    "bootstrap",
                ]),
            Self::waku_peer_list_item(editor.clone(), WakuPeerList::Backup, waku_backup_peers)
                .keywords([
                    "waku",
                    "waku connectivity",
                    "multiaddr",
                    "peer id",
                    "fallback",
                ]),
        ]
    }

    fn waku_dns_resolution_items(&self, editor: &Entity<Self>) -> [SettingItem; 2] {
        let waku_doh = Self::shared_string_field(
            "waku-doh-endpoint",
            editor.clone(),
            display_waku_doh_endpoint,
            |settings, value| settings.waku.doh_endpoint = non_empty_setting(&value),
        );
        let waku_doh_fallback_kind = SettingsUrlListKind::WakuDohFallback;
        let waku_doh_fallback_endpoints = waku_doh_fallback_kind.endpoints(&self.draft);
        [
            SettingItem::new("DoH endpoint", waku_doh)
                .keywords(["waku", "waku connectivity", "dns over https", "resolver"])
                .layout(Axis::Vertical),
            Self::settings_url_list_item(
                "DoH fallback endpoints",
                editor.clone(),
                waku_doh_fallback_kind,
                waku_doh_fallback_endpoints,
            )
            .keywords(["waku", "waku connectivity", "dns over https", "resolver"]),
        ]
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    fn waku_connection_items(editor: &Entity<Self>) -> [SettingItem; 3] {
        let waku_max_peers = Self::number_field(
            "waku-max-peers",
            editor.clone(),
            POSITIVE_NUMBER_OPTIONS,
            None,
            |settings| settings.waku.max_peers as f64,
            |settings, value| settings.waku.max_peers = value as usize,
        );
        let waku_timeout = Self::number_field(
            "waku-peer-timeout-seconds",
            editor.clone(),
            POSITIVE_NUMBER_OPTIONS,
            Some("s"),
            |settings| settings.waku.peer_connection_timeout_secs as f64,
            |settings, value| settings.waku.peer_connection_timeout_secs = value as u64,
        );
        let waku_nwaku = Self::shared_string_field(
            "waku-nwaku-rest-url",
            editor.clone(),
            |settings| settings.waku.nwaku_url.clone().unwrap_or_default(),
            |settings, value| settings.waku.nwaku_url = non_empty_setting(&value),
        );
        [
            SettingItem::new("Max peers", waku_max_peers).keywords([
                "waku",
                "waku connectivity",
                "connections",
                "limit",
            ]),
            SettingItem::new("Peer timeout", waku_timeout).keywords([
                "waku",
                "waku connectivity",
                "seconds",
                "connection",
            ]),
            SettingItem::new("nwaku REST URL", waku_nwaku)
                .keywords(["waku", "waku connectivity", "node", "rest api", "endpoint"])
                .layout(Axis::Vertical),
        ]
    }
}
