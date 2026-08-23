use super::*;

#[derive(Clone)]
pub(in crate::root) struct StartupSettingsSummary {
    pub(in crate::root) rows: Vec<(&'static str, String)>,
    pub(in crate::root) error: Option<String>,
}

impl StartupSettingsSummary {
    pub(in crate::root) const fn error(message: String) -> Self {
        Self {
            rows: Vec::new(),
            error: Some(message),
        }
    }

    pub(in crate::root) fn render(&self) -> gpui::Div {
        if let Some(error) = self.error.as_ref() {
            return div()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::DANGER))
                .bg(rgb(theme::SURFACE))
                .p(px(12.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(error.clone()));
        }

        self.rows.iter().fold(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER))
                .bg(rgb(theme::SURFACE))
                .p(px(12.0)),
            |body, (label, value)| {
                body.child(
                    div()
                        .flex()
                        .justify_between()
                        .gap_3()
                        .child(app_muted_text(*label))
                        .child(
                            div()
                                .text_color(rgb(theme::TEXT))
                                .child(SharedString::from(value.clone())),
                        ),
                )
            },
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) struct StartupSettingsActionState {
    pub(in crate::root) settings: bool,
    pub(in crate::root) reset: bool,
    pub(in crate::root) retry: bool,
    pub(in crate::root) maintenance_actions_enabled: bool,
}

pub(in crate::root) const fn startup_settings_action_state(
    has_error: bool,
    maintenance_idle: bool,
) -> StartupSettingsActionState {
    StartupSettingsActionState {
        settings: true,
        reset: has_error,
        retry: has_error,
        maintenance_actions_enabled: maintenance_idle,
    }
}

#[derive(Clone)]
pub(in crate::root) struct ProverCacheBuildParams {
    pub(in crate::root) db: Arc<WalletDbStore>,
    pub(in crate::root) db_path: PathBuf,
    pub(in crate::root) network_mode: WalletNetworkMode,
    pub(in crate::root) proxy: Option<reqwest::Url>,
    pub(in crate::root) reusable_http: Option<HttpContext>,
}

pub(in crate::root) struct PreparedProverCacheBuild {
    pub(in crate::root) params: ProverCacheBuildParams,
    pub(in crate::root) reuse_active_network: bool,
}

pub(in crate::root) struct WalletSettingsEditor {
    pub(in crate::root) vault_store: Arc<DesktopVaultStore>,
    pub(in crate::root) runtime: Handle,
    pub(in crate::root) saved: WalletSettings,
    pub(in crate::root) draft: WalletSettings,
    pub(in crate::root) field_sync_revision: u64,
    pub(in crate::root) validation_error: Option<Arc<str>>,
    pub(in crate::root) status: Option<Arc<str>>,
    pub(in crate::root) cache_building: bool,
    pub(in crate::root) cache_build_progress: Option<ProverCacheBuildProgress>,
    pub(in crate::root) maintenance_controller: Entity<WalletMaintenanceController>,
    pub(in crate::root) startup_root: Option<WeakEntity<WalletStartupRoot>>,
    pub(in crate::root) active_root: Option<WeakEntity<WalletRoot>>,
}

pub(in crate::root) struct SyncedStringFieldState {
    pub(in crate::root) input: Entity<InputState>,
    pub(in crate::root) synced_revision: u64,
    pub(in crate::root) ignore_next_change: bool,
    pub(in crate::root) _subscription: Subscription,
}

pub(in crate::root) struct SyncedNumberFieldState {
    pub(in crate::root) input: Entity<InputState>,
    pub(in crate::root) synced_revision: u64,
    pub(in crate::root) ignore_next_change: bool,
    pub(in crate::root) _subscriptions: Vec<Subscription>,
}

pub(in crate::root) struct SyncedAnchorRangeSliderState {
    pub(in crate::root) slider: Entity<SliderState>,
    pub(in crate::root) synced_revision: u64,
    pub(in crate::root) _subscription: Subscription,
}

pub(in crate::root) const ANCHOR_BPS_SLIDER_MIN: f32 = 0.0;
pub(in crate::root) const ANCHOR_BPS_SLIDER_MAX: f32 = 100_000.0;
pub(in crate::root) const ANCHOR_BPS_SLIDER_STEP: f32 = 10.0;
pub(in crate::root) const ANCHOR_BPS_SLIDER_MAX_BPS: u64 = 100_000;
pub(in crate::root) const PROXY_WAKU_DISCLAIMER: &str = "Proxy mode disables Waku's embedded libp2p transport to prevent proxy bypass. Broadcaster discovery and public broadcaster delivery are unavailable in Proxy mode.";

#[derive(Clone)]
pub(in crate::root) enum SettingsUrlListKind {
    ChainRpc { chain_id: u64, chain_label: String },
    SponsoredRelay { chain_id: u64, chain_label: String },
    PoiGateway,
    WakuDnsEnrTree,
    WakuDohFallback,
}

impl SettingsUrlListKind {
    pub(in crate::root) const fn empty_text(&self) -> &'static str {
        match self {
            Self::ChainRpc { .. } => "No RPC endpoints configured.",
            Self::SponsoredRelay { .. } => {
                "No sponsored relays configured. Sponsored self-broadcast is disabled."
            }
            Self::PoiGateway => "No artifact gateways configured.",
            Self::WakuDnsEnrTree => "No DNS ENR trees configured. DNS bootstrap is disabled.",
            Self::WakuDohFallback => "No DoH fallback endpoints configured.",
        }
    }

    pub(in crate::root) const fn dialog_help(&self) -> &'static str {
        match self {
            Self::ChainRpc { .. } => "Enter an HTTP(S) RPC endpoint for this chain.",
            Self::SponsoredRelay { .. } => {
                "Enter a compatible HTTP(S) pending-block eth_sendBundle relay."
            }
            Self::PoiGateway => {
                "Enter an HTTP(S) gateway URL for POI, indexed artifact, and governance document reads."
            }
            Self::WakuDnsEnrTree => "Enter an enrtree:// DNS discovery tree URL.",
            Self::WakuDohFallback => "Enter an HTTP(S) DNS-over-HTTPS fallback endpoint.",
        }
    }

    pub(in crate::root) fn add_id(&self) -> SharedString {
        SharedString::from(match self {
            Self::ChainRpc { chain_id, .. } => format!("wallet-settings-rpc-add-{chain_id}"),
            Self::SponsoredRelay { chain_id, .. } => {
                format!("wallet-settings-sponsored-relay-add-{chain_id}")
            }
            Self::PoiGateway => "wallet-settings-poi-gateway-add".to_string(),
            Self::WakuDnsEnrTree => "wallet-settings-waku-dns-enr-tree-add".to_string(),
            Self::WakuDohFallback => "wallet-settings-waku-doh-fallback-add".to_string(),
        })
    }

    pub(in crate::root) fn row_id(&self, index: usize) -> SharedString {
        SharedString::from(match self {
            Self::ChainRpc { chain_id, .. } => {
                format!("wallet-settings-rpc-row-{chain_id}-{index}")
            }
            Self::SponsoredRelay { chain_id, .. } => {
                format!("wallet-settings-sponsored-relay-row-{chain_id}-{index}")
            }
            Self::PoiGateway => format!("wallet-settings-poi-gateway-row-{index}"),
            Self::WakuDnsEnrTree => format!("wallet-settings-waku-dns-enr-tree-row-{index}"),
            Self::WakuDohFallback => format!("wallet-settings-waku-doh-fallback-row-{index}"),
        })
    }

    pub(in crate::root) fn edit_id(&self, index: usize) -> SharedString {
        SharedString::from(match self {
            Self::ChainRpc { chain_id, .. } => {
                format!("wallet-settings-rpc-edit-{chain_id}-{index}")
            }
            Self::SponsoredRelay { chain_id, .. } => {
                format!("wallet-settings-sponsored-relay-edit-{chain_id}-{index}")
            }
            Self::PoiGateway => format!("wallet-settings-poi-gateway-edit-{index}"),
            Self::WakuDnsEnrTree => format!("wallet-settings-waku-dns-enr-tree-edit-{index}"),
            Self::WakuDohFallback => format!("wallet-settings-waku-doh-fallback-edit-{index}"),
        })
    }

    pub(in crate::root) fn remove_id(&self, index: usize) -> SharedString {
        SharedString::from(match self {
            Self::ChainRpc { chain_id, .. } => {
                format!("wallet-settings-rpc-remove-{chain_id}-{index}")
            }
            Self::SponsoredRelay { chain_id, .. } => {
                format!("wallet-settings-sponsored-relay-remove-{chain_id}-{index}")
            }
            Self::PoiGateway => format!("wallet-settings-poi-gateway-remove-{index}"),
            Self::WakuDnsEnrTree => format!("wallet-settings-waku-dns-enr-tree-remove-{index}"),
            Self::WakuDohFallback => format!("wallet-settings-waku-doh-fallback-remove-{index}"),
        })
    }

    pub(in crate::root) fn dialog_title(&self, is_edit: bool) -> String {
        match self {
            Self::ChainRpc { chain_label, .. } => {
                if is_edit {
                    format!("Edit {chain_label} RPC")
                } else {
                    format!("Add {chain_label} RPC")
                }
            }
            Self::SponsoredRelay { chain_label, .. } => {
                if is_edit {
                    format!("Edit {chain_label} sponsored relay")
                } else {
                    format!("Add {chain_label} sponsored relay")
                }
            }
            Self::PoiGateway => {
                if is_edit {
                    "Edit artifact gateway".to_string()
                } else {
                    "Add artifact gateway".to_string()
                }
            }
            Self::WakuDnsEnrTree => {
                if is_edit {
                    "Edit DNS ENR tree".to_string()
                } else {
                    "Add DNS ENR tree".to_string()
                }
            }
            Self::WakuDohFallback => {
                if is_edit {
                    "Edit DoH fallback endpoint".to_string()
                } else {
                    "Add DoH fallback endpoint".to_string()
                }
            }
        }
    }

    pub(in crate::root) fn endpoints(&self, settings: &WalletSettings) -> Vec<String> {
        match self {
            Self::ChainRpc { chain_id, .. } => display_chain_rpc_endpoints(settings, *chain_id),
            Self::SponsoredRelay { chain_id, .. } => {
                display_sponsored_bundle_relays(settings, *chain_id)
            }
            Self::PoiGateway => settings.poi.artifact.gateway_urls.clone(),
            Self::WakuDnsEnrTree => display_waku_dns_enr_trees(settings),
            Self::WakuDohFallback => display_waku_doh_fallback_endpoints(settings),
        }
    }

    pub(in crate::root) fn set_endpoint(
        &self,
        settings: &mut WalletSettings,
        index: usize,
        value: &str,
    ) {
        match self {
            Self::ChainRpc { chain_id, .. } => {
                set_chain_rpc_endpoint(settings, *chain_id, index, value);
            }
            Self::SponsoredRelay { chain_id, .. } => {
                set_sponsored_bundle_relay(settings, *chain_id, index, value);
            }
            Self::PoiGateway => set_poi_gateway_url(settings, index, value),
            Self::WakuDnsEnrTree => set_waku_dns_enr_tree(settings, index, value),
            Self::WakuDohFallback => set_waku_doh_fallback_endpoint(settings, index, value),
        }
    }

    pub(in crate::root) fn add_endpoint(&self, settings: &mut WalletSettings, value: &str) {
        match self {
            Self::ChainRpc { chain_id, .. } => add_chain_rpc_endpoint(settings, *chain_id, value),
            Self::SponsoredRelay { chain_id, .. } => {
                add_sponsored_bundle_relay(settings, *chain_id, value);
            }
            Self::PoiGateway => add_poi_gateway_url(settings, value),
            Self::WakuDnsEnrTree => add_waku_dns_enr_tree(settings, value),
            Self::WakuDohFallback => add_waku_doh_fallback_endpoint(settings, value),
        }
    }

    pub(in crate::root) fn remove_endpoint(&self, settings: &mut WalletSettings, index: usize) {
        match self {
            Self::ChainRpc { chain_id, .. } => {
                remove_chain_rpc_endpoint(settings, *chain_id, index);
            }
            Self::SponsoredRelay { chain_id, .. } => {
                remove_sponsored_bundle_relay(settings, *chain_id, index);
            }
            Self::PoiGateway => remove_poi_gateway_url(settings, index),
            Self::WakuDnsEnrTree => remove_waku_dns_enr_tree(settings, index),
            Self::WakuDohFallback => remove_waku_doh_fallback_endpoint(settings, index),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct DisplayTokenEntry {
    pub(in crate::root) chain_id: u64,
    pub(in crate::root) token_address: String,
    pub(in crate::root) symbol: String,
    pub(in crate::root) decimals: u8,
    pub(in crate::root) icon_path: Option<String>,
    pub(in crate::root) built_in: bool,
    pub(in crate::root) custom_index: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct DisplayPriceAnchorEntry {
    pub(in crate::root) key: TokenKey,
    pub(in crate::root) price_anchor: PriceAnchorSettings,
    pub(in crate::root) token_symbol: Option<String>,
    pub(in crate::root) built_in_default: bool,
    pub(in crate::root) override_index: Option<usize>,
}

#[derive(Clone)]
pub(in crate::root) enum TokenEditTarget {
    AddCustom,
    BuiltIn(TokenKey),
    Custom(usize),
}

#[derive(Clone)]
pub(in crate::root) enum PriceAnchorEditTarget {
    Add,
    Edit(DisplayPriceAnchorEntry),
}

#[derive(Clone)]
pub(in crate::root) struct TokenDialogValues {
    pub(in crate::root) chain_id: u64,
    pub(in crate::root) token_address: String,
    pub(in crate::root) symbol: String,
    pub(in crate::root) decimals: u8,
    pub(in crate::root) icon_path: Option<String>,
}

#[derive(Clone)]
pub(in crate::root) struct TokenDialogInputs {
    pub(in crate::root) chain_id: Entity<InputState>,
    pub(in crate::root) token_address: Entity<InputState>,
    pub(in crate::root) symbol: Entity<InputState>,
    pub(in crate::root) decimals: Entity<InputState>,
    pub(in crate::root) icon_path: Entity<InputState>,
}

#[derive(Clone)]
pub(in crate::root) struct WakuDirectPeerDialogInputs {
    pub(in crate::root) peer_id: Entity<InputState>,
    pub(in crate::root) addr: Entity<InputState>,
}

#[derive(Clone)]
pub(in crate::root) struct PriceAnchorDialogInputs {
    pub(in crate::root) chain_id: Entity<SelectState<Vec<ChainSelectItem>>>,
    pub(in crate::root) token_address: Entity<InputState>,
    pub(in crate::root) anchor_type: Entity<SelectState<Vec<PriceAnchorTypeSelectItem>>>,
    pub(in crate::root) selected_anchor_type: Entity<InputState>,
    pub(in crate::root) fixed_rate: Entity<InputState>,
    pub(in crate::root) oracle_chain_id: Entity<SelectState<Vec<ChainSelectItem>>>,
    pub(in crate::root) oracle_address: Entity<InputState>,
    pub(in crate::root) oracle_token_decimals: Entity<InputState>,
    pub(in crate::root) oracle_decimals: Entity<InputState>,
    pub(in crate::root) oracle_is_inversed: Entity<SelectState<Vec<BoolSelectItem>>>,
    pub(in crate::root) twap_pool_address: Entity<InputState>,
    pub(in crate::root) twap_base_token_address: Entity<InputState>,
    pub(in crate::root) twap_quote_token_address: Entity<InputState>,
    pub(in crate::root) twap_base_token_decimals: Entity<InputState>,
    pub(in crate::root) twap_window_seconds: Entity<InputState>,
    pub(in crate::root) product_scale_decimals: Entity<InputState>,
    pub(in crate::root) product_components: Vec<ProductAnchorComponentDialogInputs>,
}

#[derive(Clone)]
pub(in crate::root) struct ProductAnchorComponentDialogInputs {
    pub(in crate::root) anchor_type: Entity<SelectState<Vec<PriceAnchorTypeSelectItem>>>,
    pub(in crate::root) selected_anchor_type: Entity<InputState>,
    pub(in crate::root) fixed_rate: Entity<InputState>,
    pub(in crate::root) oracle_chain_id: Entity<SelectState<Vec<ChainSelectItem>>>,
    pub(in crate::root) oracle_address: Entity<InputState>,
    pub(in crate::root) oracle_token_decimals: Entity<InputState>,
    pub(in crate::root) oracle_decimals: Entity<InputState>,
    pub(in crate::root) oracle_is_inversed: Entity<SelectState<Vec<BoolSelectItem>>>,
    pub(in crate::root) twap_pool_address: Entity<InputState>,
    pub(in crate::root) twap_base_token_address: Entity<InputState>,
    pub(in crate::root) twap_quote_token_address: Entity<InputState>,
    pub(in crate::root) twap_base_token_decimals: Entity<InputState>,
    pub(in crate::root) twap_window_seconds: Entity<InputState>,
}

#[derive(Clone, Copy)]
pub(in crate::root) struct PriceAnchorTypeSelectItem {
    pub(in crate::root) value: &'static str,
    pub(in crate::root) label: &'static str,
}

impl SelectItem for PriceAnchorTypeSelectItem {
    type Value = &'static str;

    fn title(&self) -> SharedString {
        SharedString::from(self.label)
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

#[derive(Clone, Copy)]
pub(in crate::root) struct BoolSelectItem {
    pub(in crate::root) value: bool,
    pub(in crate::root) label: &'static str,
}

impl SelectItem for BoolSelectItem {
    type Value = bool;

    fn title(&self) -> SharedString {
        SharedString::from(self.label)
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

#[derive(Clone, Debug)]
pub(in crate::root) struct PriceAnchorDialogValues {
    pub(in crate::root) chain_id: u64,
    pub(in crate::root) token_address: String,
    pub(in crate::root) anchor_type: &'static str,
    pub(in crate::root) fixed_rate: String,
    pub(in crate::root) oracle_chain_id: u64,
    pub(in crate::root) oracle_address: String,
    pub(in crate::root) oracle_token_decimals: String,
    pub(in crate::root) oracle_decimals: String,
    pub(in crate::root) oracle_is_inversed: bool,
    pub(in crate::root) twap_pool_address: String,
    pub(in crate::root) twap_base_token_address: String,
    pub(in crate::root) twap_quote_token_address: String,
    pub(in crate::root) twap_base_token_decimals: String,
    pub(in crate::root) twap_window_seconds: String,
    pub(in crate::root) product_scale_decimals: String,
    pub(in crate::root) product_components: Vec<PriceAnchorComponentDialogValues>,
}

#[derive(Clone, Debug)]
pub(in crate::root) struct PriceAnchorComponentDialogValues {
    pub(in crate::root) anchor_type: &'static str,
    pub(in crate::root) fixed_rate: String,
    pub(in crate::root) oracle_chain_id: u64,
    pub(in crate::root) oracle_address: String,
    pub(in crate::root) oracle_token_decimals: String,
    pub(in crate::root) oracle_decimals: String,
    pub(in crate::root) oracle_is_inversed: bool,
    pub(in crate::root) twap_pool_address: String,
    pub(in crate::root) twap_base_token_address: String,
    pub(in crate::root) twap_quote_token_address: String,
    pub(in crate::root) twap_base_token_decimals: String,
    pub(in crate::root) twap_window_seconds: String,
}
