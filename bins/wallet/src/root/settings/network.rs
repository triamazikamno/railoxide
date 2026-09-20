use super::*;

pub(in crate::root) fn display_waku_doh_endpoint(settings: &WalletSettings) -> String {
    settings
        .waku
        .doh_endpoint
        .clone()
        .unwrap_or_else(|| default_waku_doh_endpoint(settings.network.mode).to_string())
}

pub(in crate::root) const fn default_waku_doh_endpoint(mode: NetworkModeSetting) -> &'static str {
    match mode {
        NetworkModeSetting::Tor => DEFAULT_TOR_DOH_ENDPOINT,
        NetworkModeSetting::Proxy | NetworkModeSetting::Direct => DEFAULT_DOH_ENDPOINT,
    }
}

pub(in crate::root) fn display_waku_doh_fallback_endpoints(
    settings: &WalletSettings,
) -> Vec<String> {
    settings
        .waku
        .doh_fallback_endpoints
        .clone()
        .unwrap_or_else(|| default_waku_doh_fallback_endpoints(settings.network.mode))
}

pub(in crate::root) fn display_waku_dns_enr_trees(settings: &WalletSettings) -> Vec<String> {
    settings
        .waku
        .dns_enr_trees
        .clone()
        .unwrap_or_else(default_waku_dns_enr_trees)
}

impl WakuPeerList {
    pub(in crate::root) const fn id(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Backup => "backup",
        }
    }

    pub(in crate::root) fn peers(self, settings: &WalletSettings) -> Vec<WakuDirectPeerSetting> {
        match self {
            Self::Direct => settings
                .waku
                .direct_peers
                .clone()
                .unwrap_or_else(default_waku_direct_peers),
            Self::Backup => settings
                .waku
                .backup_peers
                .clone()
                .unwrap_or_else(default_waku_backup_peers),
        }
    }

    fn peers_mut(self, settings: &mut WalletSettings) -> &mut Vec<WakuDirectPeerSetting> {
        match self {
            Self::Direct => settings
                .waku
                .direct_peers
                .get_or_insert_with(default_waku_direct_peers),
            Self::Backup => settings
                .waku
                .backup_peers
                .get_or_insert_with(default_waku_backup_peers),
        }
    }
}

pub(in crate::root) fn default_waku_doh_fallback_endpoints(
    mode: NetworkModeSetting,
) -> Vec<String> {
    match mode {
        NetworkModeSetting::Tor => vec![DEFAULT_DOH_ENDPOINT.to_string()],
        NetworkModeSetting::Proxy | NetworkModeSetting::Direct => Vec::new(),
    }
}

pub(in crate::root) fn materialize_waku_doh_fallback_endpoints(
    settings: &mut WalletSettings,
) -> &mut Vec<String> {
    if settings.waku.doh_fallback_endpoints.is_none() {
        settings.waku.doh_fallback_endpoints =
            Some(default_waku_doh_fallback_endpoints(settings.network.mode));
    }
    settings
        .waku
        .doh_fallback_endpoints
        .as_mut()
        .expect("fallback endpoints were just initialized")
}

pub(in crate::root) fn materialize_waku_dns_enr_trees(
    settings: &mut WalletSettings,
) -> &mut Vec<String> {
    if settings.waku.dns_enr_trees.is_none() {
        settings.waku.dns_enr_trees = Some(default_waku_dns_enr_trees());
    }
    settings
        .waku
        .dns_enr_trees
        .as_mut()
        .expect("DNS ENR trees were just initialized")
}

pub(in crate::root) fn set_waku_doh_fallback_endpoint(
    settings: &mut WalletSettings,
    index: usize,
    value: &str,
) {
    let endpoints = materialize_waku_doh_fallback_endpoints(settings);
    if endpoints.len() <= index {
        endpoints.resize(index + 1, String::new());
    }
    endpoints[index] = value.trim().to_string();
}

pub(in crate::root) fn set_waku_dns_enr_tree(
    settings: &mut WalletSettings,
    index: usize,
    value: &str,
) {
    let trees = materialize_waku_dns_enr_trees(settings);
    if trees.len() <= index {
        trees.resize(index + 1, String::new());
    }
    trees[index] = value.trim().to_string();
}

pub(in crate::root) fn add_waku_doh_fallback_endpoint(settings: &mut WalletSettings, value: &str) {
    materialize_waku_doh_fallback_endpoints(settings).push(value.trim().to_string());
}

pub(in crate::root) fn add_waku_dns_enr_tree(settings: &mut WalletSettings, value: &str) {
    materialize_waku_dns_enr_trees(settings).push(value.trim().to_string());
}

pub(in crate::root) fn remove_waku_doh_fallback_endpoint(
    settings: &mut WalletSettings,
    index: usize,
) {
    let endpoints = materialize_waku_doh_fallback_endpoints(settings);
    if index < endpoints.len() {
        endpoints.remove(index);
    }
}

pub(in crate::root) fn remove_waku_dns_enr_tree(settings: &mut WalletSettings, index: usize) {
    let trees = materialize_waku_dns_enr_trees(settings);
    if index < trees.len() {
        trees.remove(index);
    }
}

pub(in crate::root) fn set_waku_peer(
    settings: &mut WalletSettings,
    kind: WakuPeerList,
    index: usize,
    peer: WakuDirectPeerSetting,
) {
    let peers = kind.peers_mut(settings);
    if peers.len() <= index {
        peers.resize(index + 1, WakuDirectPeerSetting::default());
    }
    peers[index] = peer;
}

pub(in crate::root) fn add_waku_peer(
    settings: &mut WalletSettings,
    kind: WakuPeerList,
    peer: WakuDirectPeerSetting,
) {
    kind.peers_mut(settings).push(peer);
}

pub(in crate::root) fn remove_waku_peer(
    settings: &mut WalletSettings,
    kind: WakuPeerList,
    index: usize,
) {
    let peers = kind.peers_mut(settings);
    if index < peers.len() {
        peers.remove(index);
    }
}

pub(in crate::root) fn set_poi_gateway_url(
    settings: &mut WalletSettings,
    index: usize,
    value: &str,
) {
    if let Some(gateway) = settings.poi.artifact.gateway_urls.get_mut(index) {
        *gateway = value.trim().to_string();
    }
}

pub(in crate::root) fn add_poi_gateway_url(settings: &mut WalletSettings, value: &str) {
    settings
        .poi
        .artifact
        .gateway_urls
        .push(value.trim().to_string());
}

pub(in crate::root) fn remove_poi_gateway_url(settings: &mut WalletSettings, index: usize) {
    if index < settings.poi.artifact.gateway_urls.len() {
        settings.poi.artifact.gateway_urls.remove(index);
    }
}

pub(in crate::root) const fn indexed_artifact_source_mode_value(
    mode: IndexedArtifactSourceModeSetting,
) -> &'static str {
    match mode {
        IndexedArtifactSourceModeSetting::Disabled => "disabled",
        IndexedArtifactSourceModeSetting::Official => "official",
        IndexedArtifactSourceModeSetting::Custom => "custom",
    }
}

pub(in crate::root) fn apply_indexed_artifact_source_mode(
    settings: &mut WalletSettings,
    value: &str,
) {
    match value {
        "official" => settings.indexed_artifacts = IndexedArtifactSettings::official_preset(),
        "custom" => {
            settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
        }
        _ => settings.indexed_artifacts = IndexedArtifactSettings::disabled_preset(),
    }
}

pub(in crate::root) const fn indexed_artifact_source_status_message(
    settings: &WalletSettings,
) -> &'static str {
    match settings.indexed_artifacts.source_mode {
        IndexedArtifactSourceModeSetting::Disabled => "Squid quick-sync -> RPC",
        IndexedArtifactSourceModeSetting::Official => {
            "Official indexed artifacts -> Squid quick-sync -> RPC"
        }
        IndexedArtifactSourceModeSetting::Custom => {
            "Custom indexed artifacts -> Squid quick-sync -> RPC"
        }
    }
}

pub(in crate::root) const fn should_show_indexed_artifact_custom_settings(
    mode: IndexedArtifactSourceModeSetting,
) -> bool {
    matches!(mode, IndexedArtifactSourceModeSetting::Custom)
}

pub(in crate::root) const fn network_mode_value(mode: NetworkModeSetting) -> &'static str {
    match mode {
        NetworkModeSetting::Tor => "tor",
        NetworkModeSetting::Proxy => "proxy",
        NetworkModeSetting::Direct => "direct",
    }
}

pub(in crate::root) fn network_mode_from_value(value: &str) -> NetworkModeSetting {
    match value {
        "proxy" => NetworkModeSetting::Proxy,
        "direct" => NetworkModeSetting::Direct,
        _ => NetworkModeSetting::Tor,
    }
}

pub(in crate::root) const fn should_show_proxy_url_setting(mode: NetworkModeSetting) -> bool {
    matches!(mode, NetworkModeSetting::Proxy)
}

pub(in crate::root) const fn should_show_proxy_waku_disclaimer(mode: NetworkModeSetting) -> bool {
    matches!(mode, NetworkModeSetting::Proxy)
}

pub(in crate::root) const fn poi_source_value(source: PoiReadSourceSetting) -> &'static str {
    match source {
        PoiReadSourceSetting::IndexedArtifacts => "indexed-artifacts",
        PoiReadSourceSetting::PoiProxy => "poi-proxy",
    }
}

pub(in crate::root) fn poi_source_from_value(value: &str) -> PoiReadSourceSetting {
    match value {
        "poi-proxy" => PoiReadSourceSetting::PoiProxy,
        _ => PoiReadSourceSetting::IndexedArtifacts,
    }
}

pub(in crate::root) fn non_empty_setting(value: &str) -> Option<String> {
    let value = value.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

pub(in crate::root) fn parse_price_anchor_type(value: &str) -> Result<&'static str, String> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("fixed") {
        Ok("fixed")
    } else if value.eq_ignore_ascii_case("oracle") {
        Ok("oracle")
    } else if value.eq_ignore_ascii_case("uniswap-v3-twap") || value.eq_ignore_ascii_case("twap") {
        Ok("uniswap-v3-twap")
    } else if value.eq_ignore_ascii_case("product") {
        Ok("product")
    } else {
        Err("Anchor type must be fixed, oracle, or product (or uniswap-v3-twap)".to_string())
    }
}

pub(in crate::root) fn parse_product_component_anchor_type(
    value: &str,
) -> Result<&'static str, String> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("fixed") {
        Ok("fixed")
    } else if value.eq_ignore_ascii_case("oracle") {
        Ok("oracle")
    } else if value.eq_ignore_ascii_case("uniswap-v3-twap") || value.eq_ignore_ascii_case("twap") {
        Ok("uniswap-v3-twap")
    } else {
        Err("Product component type must be fixed, oracle, or uniswap-v3-twap".to_string())
    }
}

pub(in crate::root) fn default_price_anchor_for_type(value: &str) -> PriceAnchorSettings {
    match value {
        "oracle" => PriceAnchorSettings::Oracle {
            chain_id: railgun_ui::DEFAULT_CHAINS[0],
            oracle_address: Address::ZERO.to_string(),
            token_decimals: 18,
            oracle_decimals: 8,
            is_inversed: false,
        },
        "uniswap-v3-twap" => PriceAnchorSettings::UniswapV3Twap {
            pool_address: Address::ZERO.to_string(),
            base_token_address: Address::ZERO.to_string(),
            quote_token_address: Address::ZERO.to_string(),
            base_token_decimals: 18,
            window_seconds: 1_800,
        },
        "product" => PriceAnchorSettings::Product {
            components: default_product_anchor_components(),
            scale_decimals: 18,
        },
        _ => PriceAnchorSettings::default(),
    }
}

pub(in crate::root) fn default_product_anchor_components() -> Vec<PriceAnchorSettings> {
    vec![
        default_price_anchor_for_type("oracle"),
        default_price_anchor_for_type("oracle"),
    ]
}

pub(in crate::root) fn fixed_anchor_rate_value(anchor: &PriceAnchorSettings) -> String {
    match anchor {
        PriceAnchorSettings::Fixed { rate } => rate.clone(),
        _ => String::new(),
    }
}
