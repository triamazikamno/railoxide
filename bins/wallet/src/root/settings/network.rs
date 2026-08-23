use super::*;

pub(in crate::root) fn display_chain_rpc_endpoints(
    settings: &WalletSettings,
    chain_id: u64,
) -> Vec<String> {
    settings
        .chains
        .per_chain
        .get(&chain_id)
        .filter(|chain| !chain.rpc_endpoints.is_empty())
        .map_or_else(
            || default_chain_rpc_endpoints(chain_id).unwrap_or_default(),
            |chain| chain.rpc_endpoints.clone(),
        )
}

pub(in crate::root) fn display_sponsored_bundle_relays(
    settings: &WalletSettings,
    chain_id: u64,
) -> Vec<String> {
    settings
        .chains
        .per_chain
        .get(&chain_id)
        .and_then(|chain| chain.sponsored_bundle_relays.clone())
        .unwrap_or_else(|| default_sponsored_bundle_relay_endpoints(chain_id))
}

pub(in crate::root) fn display_chain_quick_sync_endpoint(
    settings: &WalletSettings,
    chain_id: u64,
) -> String {
    settings
        .chains
        .per_chain
        .get(&chain_id)
        .and_then(|chain| chain.quick_sync.endpoint.clone())
        .unwrap_or_else(|| default_chain_quick_sync_endpoint(chain_id).unwrap_or_default())
}

pub(in crate::root) fn display_chain_contract_settings(
    settings: &WalletSettings,
    chain_id: u64,
) -> ChainContractSettings {
    let mut contracts = default_chain_contract_settings(chain_id).unwrap_or_default();
    if let Some(overrides) = settings.chains.per_chain.get(&chain_id) {
        if let Some(value) = overrides.contracts.railgun_contract.as_ref() {
            contracts.railgun_contract = Some(value.clone());
        }
        if let Some(value) = overrides.contracts.relay_adapt_contract.as_ref() {
            contracts.relay_adapt_contract = Some(value.clone());
        }
        if let Some(value) = overrides.contracts.relay_adapt_7702_contract.as_ref() {
            contracts.relay_adapt_7702_contract = Some(value.clone());
        }
        if let Some(value) = overrides.contracts.wrapped_native_token.as_ref() {
            contracts.wrapped_native_token = Some(value.clone());
        }
        if let Some(value) = overrides.contracts.multicall_contract.as_ref() {
            contracts.multicall_contract = Some(value.clone());
        }
        if let Some(value) = overrides.contracts.coinbase_payer.as_ref() {
            contracts.coinbase_payer = Some(value.clone());
        }
    }
    contracts
}

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

pub(in crate::root) fn display_waku_direct_peers(
    settings: &WalletSettings,
) -> Vec<WakuDirectPeerSetting> {
    settings
        .waku
        .direct_peers
        .clone()
        .unwrap_or_else(default_waku_direct_peers)
}

pub(in crate::root) fn default_waku_doh_fallback_endpoints(
    mode: NetworkModeSetting,
) -> Vec<String> {
    match mode {
        NetworkModeSetting::Tor => vec![DEFAULT_DOH_ENDPOINT.to_string()],
        NetworkModeSetting::Proxy | NetworkModeSetting::Direct => Vec::new(),
    }
}

pub(in crate::root) fn materialize_chain_rpc_endpoints(
    settings: &mut WalletSettings,
    chain_id: u64,
) -> &mut Vec<String> {
    let chain = settings.chains.per_chain.entry(chain_id).or_default();
    if chain.rpc_endpoints.is_empty() {
        chain.rpc_endpoints = default_chain_rpc_endpoints(chain_id).unwrap_or_default();
    }
    &mut chain.rpc_endpoints
}

pub(in crate::root) fn materialize_sponsored_bundle_relays(
    settings: &mut WalletSettings,
    chain_id: u64,
) -> &mut Vec<String> {
    let chain = settings.chains.per_chain.entry(chain_id).or_default();
    if chain.sponsored_bundle_relays.is_none() {
        chain.sponsored_bundle_relays = Some(default_sponsored_bundle_relay_endpoints(chain_id));
    }
    chain
        .sponsored_bundle_relays
        .as_mut()
        .expect("sponsored relay overrides were just initialized")
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

pub(in crate::root) fn materialize_waku_direct_peers(
    settings: &mut WalletSettings,
) -> &mut Vec<WakuDirectPeerSetting> {
    if settings.waku.direct_peers.is_none() {
        settings.waku.direct_peers = Some(default_waku_direct_peers());
    }
    settings
        .waku
        .direct_peers
        .as_mut()
        .expect("direct peers were just initialized")
}

pub(in crate::root) fn set_chain_rpc_endpoint(
    settings: &mut WalletSettings,
    chain_id: u64,
    index: usize,
    value: &str,
) {
    let endpoints = materialize_chain_rpc_endpoints(settings, chain_id);
    if endpoints.len() <= index {
        endpoints.resize(index + 1, String::new());
    }
    endpoints[index] = value.trim().to_string();
}

pub(in crate::root) fn set_sponsored_bundle_relay(
    settings: &mut WalletSettings,
    chain_id: u64,
    index: usize,
    value: &str,
) {
    let relays = materialize_sponsored_bundle_relays(settings, chain_id);
    if relays.len() <= index {
        relays.resize(index + 1, String::new());
    }
    relays[index] = value.trim().to_string();
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

pub(in crate::root) fn add_chain_rpc_endpoint(
    settings: &mut WalletSettings,
    chain_id: u64,
    value: &str,
) {
    materialize_chain_rpc_endpoints(settings, chain_id).push(value.trim().to_string());
}

pub(in crate::root) fn add_sponsored_bundle_relay(
    settings: &mut WalletSettings,
    chain_id: u64,
    value: &str,
) {
    materialize_sponsored_bundle_relays(settings, chain_id).push(value.trim().to_string());
}

pub(in crate::root) fn add_waku_doh_fallback_endpoint(settings: &mut WalletSettings, value: &str) {
    materialize_waku_doh_fallback_endpoints(settings).push(value.trim().to_string());
}

pub(in crate::root) fn add_waku_dns_enr_tree(settings: &mut WalletSettings, value: &str) {
    materialize_waku_dns_enr_trees(settings).push(value.trim().to_string());
}

pub(in crate::root) fn remove_chain_rpc_endpoint(
    settings: &mut WalletSettings,
    chain_id: u64,
    index: usize,
) {
    let endpoints = materialize_chain_rpc_endpoints(settings, chain_id);
    if index < endpoints.len() {
        endpoints.remove(index);
    }
}

pub(in crate::root) fn remove_sponsored_bundle_relay(
    settings: &mut WalletSettings,
    chain_id: u64,
    index: usize,
) {
    let relays = materialize_sponsored_bundle_relays(settings, chain_id);
    if index < relays.len() {
        relays.remove(index);
    }
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

pub(in crate::root) fn set_waku_direct_peer(
    settings: &mut WalletSettings,
    index: usize,
    peer: WakuDirectPeerSetting,
) {
    let peers = materialize_waku_direct_peers(settings);
    if peers.len() <= index {
        peers.resize(index + 1, WakuDirectPeerSetting::default());
    }
    peers[index] = peer;
}

pub(in crate::root) fn add_waku_direct_peer(
    settings: &mut WalletSettings,
    peer: WakuDirectPeerSetting,
) {
    materialize_waku_direct_peers(settings).push(peer);
}

pub(in crate::root) fn remove_waku_direct_peer(settings: &mut WalletSettings, index: usize) {
    let peers = materialize_waku_direct_peers(settings);
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

pub(in crate::root) fn optional_u64_setting(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        value.parse().ok()
    }
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
