use super::*;
use wallet_ops::device_auth::DeviceAuthMethod;

const INDEXED_ARTIFACT_REPOSITORY_URL: &str = "https://github.com/triamazikamno/railgun-indexer/";

const MIB: u64 = 1024 * 1024;

fn indexed_artifact_repository_help() -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1()
        .text_size(px(13.0))
        .line_height(px(18.0))
        .text_color(rgb(theme::TEXT_SUBTLE))
        .child(
            "Chain-indexed data artifacts for wallet catch-up, public TXID cache, and Merkle quick-sync. See",
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .font_family(APP_MONO_FONT_FAMILY)
                .text_color(rgb(theme::TEXT_MUTED))
                .child(INDEXED_ARTIFACT_REPOSITORY_URL)
                .child(clipboard_with_toast(
                    "wallet-settings-indexed-artifact-repository-url-copy",
                    INDEXED_ARTIFACT_REPOSITORY_URL,
                )),
        )
        .child("for more information or if you want to run your own")
}

impl Render for WalletSettingsEditor {
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let editor = cx.entity();
        let root_replacement_allowed = self.root_replacement_is_allowed(cx);
        let chain_editing = self.chain_editor.read(cx).is_editing();
        let open_page = self.open_page.take();
        // Before network startup nothing needs restarting, so Save takes every valid change.
        let save_enabled = if self.pre_start {
            self.validation_error.is_none() && self.draft != self.saved
        } else {
            settings_save_action_enabled(&self.saved, &self.draft, self.validation_error.is_some())
        };
        let auto_lock_timeout = Self::dropdown_field(
            editor.clone(),
            auto_lock_timeout_options(),
            |settings| auto_lock_timeout_value(settings.runtime.auto_lock_timeout_secs),
            |settings, value| {
                settings.runtime.auto_lock_timeout_secs =
                    auto_lock_timeout_from_value(value.as_ref());
            },
        );
        let network_mode = Self::dropdown_field(
            editor.clone(),
            vec![
                (
                    SharedString::from("tor"),
                    SharedString::from("Built-in Tor"),
                ),
                (SharedString::from("proxy"), SharedString::from("Proxy")),
                (SharedString::from("direct"), SharedString::from("Direct")),
            ],
            |settings| SharedString::from(network_mode_value(settings.network.mode)),
            |settings, value| {
                settings.network.mode = network_mode_from_value(value.as_ref());
                if !should_show_proxy_url_setting(settings.network.mode) {
                    settings.network.proxy_url = None;
                }
            },
        );
        let proxy_url = Self::shared_string_field(
            "network-proxy-url",
            editor.clone(),
            |settings| settings.network.proxy_url.clone().unwrap_or_default(),
            |settings, value| {
                settings.network.proxy_url = non_empty_setting(&value);
            },
        );
        let poi_source = Self::dropdown_field(
            editor.clone(),
            vec![
                (
                    SharedString::from("indexed-artifacts"),
                    SharedString::from("Indexed artifacts"),
                ),
                (
                    SharedString::from("poi-proxy"),
                    SharedString::from("PPOI proxy"),
                ),
            ],
            |settings| SharedString::from(poi_source_value(settings.poi.read_source)),
            |settings, value| {
                settings.poi.read_source = poi_source_from_value(value.as_ref());
            },
        );
        let poi_rpc_url = Self::shared_string_field(
            "poi-rpc-url",
            editor.clone(),
            |settings| settings.poi.proxy.rpc_url.clone(),
            |settings, value| {
                settings.poi.proxy.rpc_url = value;
            },
        );
        let poi_publisher = Self::shared_string_field(
            "poi-publisher-public-key",
            editor.clone(),
            |settings| settings.poi.artifact.publisher_pubkey.clone(),
            |settings, value| {
                settings.poi.artifact.publisher_pubkey = value;
            },
        );
        let poi_ipns = Self::shared_string_field(
            "poi-ipns-name",
            editor.clone(),
            |settings| match &settings.poi.artifact.manifest_source {
                wallet_ops::settings::PoiArtifactManifestSourceSetting::IpnsName(name) => {
                    name.clone()
                }
                _ => String::new(),
            },
            |settings, value| {
                settings.poi.artifact.manifest_source =
                    wallet_ops::settings::PoiArtifactManifestSourceSetting::IpnsName(value);
            },
        );
        let poi_publisher_choice = {
            let get_editor = editor.clone();
            let set_editor = editor.clone();
            // The choice is editor state, not a stored setting, so it reads the editor directly.
            SettingField::dropdown(
                vec![
                    (
                        SharedString::from("official"),
                        SharedString::from("Official"),
                    ),
                    (SharedString::from("custom"), SharedString::from("Custom")),
                ],
                move |cx| {
                    SharedString::from(if get_editor.read(cx).poi_custom_publisher_selected() {
                        "custom"
                    } else {
                        "official"
                    })
                },
                move |value, cx| {
                    set_editor.update(cx, |editor, cx| {
                        editor.select_poi_publisher(value.as_ref() == "custom", cx);
                    });
                },
            )
        };
        let poi_cache_reset_editor = editor.clone();
        let poi_data_reset_editor = editor.clone();
        let merkle_forest_cache_reset_editor = editor.clone();
        let prover_cache_editor = editor.clone();
        let data_folder_editor = editor.clone();
        let indexed_source_mode = Self::dropdown_field(
            editor.clone(),
            vec![
                (
                    SharedString::from("disabled"),
                    SharedString::from("Disabled"),
                ),
                (
                    SharedString::from("official"),
                    SharedString::from("Official"),
                ),
                (SharedString::from("custom"), SharedString::from("Custom")),
            ],
            |settings| {
                SharedString::from(indexed_artifact_source_mode_value(
                    settings.indexed_artifacts.source_mode,
                ))
            },
            |settings, value| apply_indexed_artifact_source_mode(settings, value.as_ref()),
        );
        let indexed_publisher = Self::shared_string_field(
            "indexed-artifact-publisher-public-key",
            editor.clone(),
            |settings| {
                settings
                    .indexed_artifacts
                    .publisher_pubkey
                    .clone()
                    .unwrap_or_default()
            },
            |settings, value| {
                settings.indexed_artifacts.publisher_pubkey = non_empty_setting(&value);
            },
        );
        let indexed_ipns = Self::shared_string_field(
            "indexed-artifact-ipns-name",
            editor.clone(),
            |settings| match settings.indexed_artifacts.manifest_source.as_ref() {
                Some(IndexedArtifactManifestSourceSetting::IpnsName(name)) => name.clone(),
                _ => String::new(),
            },
            |settings, value| {
                settings.indexed_artifacts.manifest_source = Some(
                    IndexedArtifactManifestSourceSetting::IpnsName(value.trim().to_string()),
                );
            },
        );
        let indexed_concurrency_options = NumberFieldOptions {
            min: 1.0,
            max: 32.0,
            step: 1.0,
        };
        let indexed_byte_budget_options = NumberFieldOptions {
            min: 1.0,
            max: 1024.0,
            step: 1.0,
        };
        let indexed_concurrency = Self::number_field(
            "indexed-artifact-concurrency",
            editor.clone(),
            indexed_concurrency_options,
            None,
            |settings| {
                settings
                    .indexed_artifacts
                    .concurrency
                    .unwrap_or(wallet_ops::settings::DEFAULT_INDEXED_ARTIFACT_CONCURRENCY)
                    as f64
            },
            |settings, value| settings.indexed_artifacts.concurrency = Some(value as usize),
        );
        let indexed_byte_budget =
            Self::number_field(
                "indexed-artifact-byte-budget",
                editor.clone(),
                indexed_byte_budget_options,
                Some("MiB"),
                |settings| {
                    (settings.indexed_artifacts.max_in_flight_bytes.unwrap_or(
                        wallet_ops::settings::DEFAULT_INDEXED_ARTIFACT_MAX_IN_FLIGHT_BYTES,
                    ) / MIB) as f64
                },
                |settings, value| {
                    settings.indexed_artifacts.max_in_flight_bytes = Some(value as u64 * MIB);
                },
            );
        let indexed_reset_editor = editor.clone();
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
        let walletconnect_project_id = Self::shared_string_field(
            "walletconnect-project-id-override",
            editor.clone(),
            |settings| settings.walletconnect.effective_project_id().to_owned(),
            |settings, value| {
                settings.walletconnect.project_id_override = non_empty_setting(&value)
                    .filter(|project_id| project_id.as_str() != WALLETCONNECT_DEFAULT_PROJECT_ID);
            },
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

        let indexed_artifact_status = SharedString::from(format!(
            "Current index source priority order: {}",
            indexed_artifact_source_status_message(&self.draft)
        ));
        let mut indexed_artifact_group = settings_group()
            .title("Indexed artifacts")
            .item(
                SettingItem::new("Indexed artifact source", indexed_source_mode).keywords([
                    "indexed artifacts",
                    "disabled",
                    "official",
                    "custom",
                    "publisher",
                    "public key",
                    "ipns",
                    "concurrency",
                    "byte budget",
                    "quick-sync",
                    "ipfs",
                    "data sources",
                ]),
            )
            .footer(move |_window, _cx| {
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(settings_info_banner(indexed_artifact_status.clone()))
                    .child(indexed_artifact_repository_help())
            });
        if should_show_indexed_artifact_custom_settings(self.draft.indexed_artifacts.source_mode) {
            indexed_artifact_group = indexed_artifact_group
                .item(
                    SettingItem::new("Publisher public key", indexed_publisher)
                        .keywords(["indexed artifacts", "publisher", "signing key", "custom"])
                        .layout(Axis::Vertical),
                )
                .item(
                    SettingItem::new("IPNS name", indexed_ipns)
                        .keywords(["indexed artifacts", "ipfs", "manifest", "custom"])
                        .layout(Axis::Vertical),
                )
                .item(
                    SettingItem::new("Chunk concurrency", indexed_concurrency).keywords([
                        "indexed artifacts",
                        "parallel",
                        "downloads",
                        "chunks",
                    ]),
                )
                .item(
                    SettingItem::new("In-flight byte budget", indexed_byte_budget).keywords([
                        "indexed artifacts",
                        "memory",
                        "downloads",
                        "megabytes",
                    ]),
                )
                .item(
                    SettingItem::new(
                        "Reset official indexed artifacts",
                        SettingField::<SharedString>::render(move |_options, _window, _cx| {
                            let reset_editor = indexed_reset_editor.clone();
                            app_button(
                                "wallet-settings-indexed-artifact-official-preset",
                                "Reset to official",
                            )
                            .on_click(move |_event, _window, cx| {
                                reset_editor.update(cx, |editor, cx| {
                                    editor.draft.indexed_artifacts =
                                        IndexedArtifactSettings::official_preset();
                                    editor.programmatic_draft_changed(cx);
                                });
                            })
                        }),
                    )
                    .keywords([
                        "indexed artifacts",
                        "preset",
                        "defaults",
                        "custom",
                    ]),
                );
        }

        let mut poi_group = settings_group()
            .title("PPOI")
            .item(
                SettingItem::new("PPOI source", poi_source)
                    .description("'Indexed artifacts' downloads snapshots containing PPOI data from IPFS and uses the PPOI RPC URL only to live-tail recent public PPOI events. 'PPOI proxy' mode is less private: the PPOI RPC receives requests containing blind commitment hashes associated with UTXOs you are receiving or preparing to spend. Use PPOI proxy mode only if you trust the PPOI RPC operator.")
                    .keywords([
                        "ppoi",
                        "poi",
                        "proof of innocence",
                        "indexed artifacts",
                        "ppoi proxy",
                        "publisher",
                        "public key",
                        "ipns",
                        "data sources",
                    ]),
            )
            .item(
                SettingItem::new("PPOI RPC URL", poi_rpc_url)
                    .description("Used for indexed-artifact live tailing and for direct PPOI status/proof requests when PPOI proxy mode is selected.")
                    .keywords([
                        "ppoi",
                        "poi",
                        "proof of innocence",
                        "endpoint",
                        "live tail",
                    ])
                    .layout(Axis::Vertical),
            );
        if matches!(
            self.draft.poi.read_source,
            PoiReadSourceSetting::IndexedArtifacts
        ) {
            poi_group = poi_group.item(
                SettingItem::new("Publisher", poi_publisher_choice).keywords([
                    "ppoi",
                    "poi",
                    "proof of innocence",
                    "official",
                    "custom",
                    "public key",
                    "ipns",
                ]),
            );
            if self.poi_custom_publisher_selected() {
                poi_group = poi_group
                    .item(
                        SettingItem::new("Publisher public key", poi_publisher)
                            .keywords(["ppoi", "poi", "signing key", "custom"])
                            .layout(Axis::Vertical),
                    )
                    .item(
                        SettingItem::new("IPNS name", poi_ipns)
                            .keywords(["ppoi", "poi", "ipfs", "manifest", "custom"])
                            .layout(Axis::Vertical),
                    );
            }
        }

        let poi_gateway_kind = SettingsUrlListKind::PoiGateway;
        let poi_gateway_endpoints = poi_gateway_kind.endpoints(&self.draft);
        let poi_gateway_editor = editor.clone();
        let poi_gateway_group = settings_group().title("Artifact gateways").item(
            Self::settings_url_list_item(
                "Artifact gateway URLs",
                poi_gateway_editor,
                poi_gateway_kind,
                poi_gateway_endpoints,
            )
            .keywords([
                "gateways",
                "ipfs",
                "http",
                "ppoi",
                "indexed artifacts",
                "governance",
            ]),
        );
        let data_sources_page = SettingPage::new("Data sources")
            .group(indexed_artifact_group)
            .group(poi_group)
            .group(poi_gateway_group);

        let chain_editor = self.chain_editor.clone();
        let chains_page = SettingPage::new("Chains")
            .group(
                settings_group().item(
                    SettingItem::render(move |_, _, _| div().w_full().child(chain_editor.clone()))
                        .keywords(chains_page_keywords(&self.draft)),
                ),
            )
            .groups(self.tron_settings_groups(&editor));

        let mut token_page = SettingPage::new("Tokens");
        let token_entries = display_token_entries(&self.draft);
        let token_editor = editor.clone();
        token_page = token_page.group(
            settings_group().item(
                SettingItem::new(
                    "Tokens",
                    SettingField::<SharedString>::render(move |_options, _window, _cx| {
                        Self::render_token_list(&token_editor, token_entries.clone())
                    }),
                )
                .description("Known token metadata, built-in token overrides, and custom tokens.")
                .keywords([
                    "erc-20",
                    "erc20",
                    "custom token",
                    "decimals",
                    "symbol",
                    "contract address",
                ])
                .layout(Axis::Vertical),
            ),
        );

        let price_anchor_entries = display_price_anchor_entries(&self.draft);
        let price_anchor_editor = editor.clone();
        token_page = token_page.group(
            settings_group().item(
                SettingItem::new(
                    "Price oracles",
                    SettingField::<SharedString>::render(move |_options, _window, _cx| {
                        Self::render_price_anchor_list(
                            &price_anchor_editor,
                            price_anchor_entries.clone(),
                        )
                    }),
                )
                .description("Token price anchors used to evaluate transaction fees.")
                .keywords(["price feed", "usd", "chainlink", "fees", "tokens"])
                .layout(Axis::Vertical),
            ),
        );

        let save_editor = editor.clone();
        let discard_editor = editor.clone();
        let reset_editor = editor.clone();
        let apply_editor = editor.clone();
        let mut security_group = settings_group().item(
            SettingItem::new("Auto-lock vault", auto_lock_timeout)
                .description("Lock the vault after this long without wallet activity.")
                .keywords([
                    "timeout",
                    "idle",
                    "lock",
                    "autolock",
                    "inactivity",
                    "disabled",
                    "minutes",
                    "security",
                ]),
        );
        for method in DeviceAuthMethod::ALL {
            if let Some(setting) = self.device_auth_setting(method, cx) {
                security_group = security_group.item(setting);
            }
        }
        let security_page = SettingPage::new("Security").group(security_group);

        let general_page = SettingPage::new("General").group(
            settings_group()
                .item(
                    Self::settings_switch_item(
                        "wallet-settings-unwrap-by-default",
                        "Unwrap by default",
                        editor.clone(),
                        None,
                        |settings| settings.privacy.unwrap_unshields_by_default,
                        |settings, value| {
                            settings.privacy.unwrap_unshields_by_default = value;
                        },
                    )
                    .description(
                        "Preselects native token output when unshielding wrapped native tokens. You can change the output in each unshield form.",
                    )
                    .keywords(["unshield", "wrapped", "native token", "transaction defaults"]),
                )
                .item(
                    Self::settings_switch_item(
                        "wallet-settings-mimic-railway-by-default",
                        "Mimic Railway by default",
                        editor.clone(),
                        None,
                        |settings| settings.privacy.mimic_railway_shields_by_default,
                        |settings, value| {
                            settings.privacy.mimic_railway_shields_by_default = value;
                        },
                    )
                    .description(
                        "Preselects Mimic Railway for new shields. ERC-20 shields may grant an unlimited token allowance.",
                    )
                    .keywords(["shield", "allowance", "approval", "transaction defaults"]),
                ),
        );

        let mut network_group =
            settings_group().item(SettingItem::new("Network mode", network_mode).keywords([
                "tor",
                "built-in tor",
                "proxy",
                "direct",
                "proxy url",
                "socks",
                "privacy",
                "anonymity",
                "connection",
            ]));
        if should_show_proxy_url_setting(self.draft.network.mode) {
            network_group = network_group.item(
                SettingItem::new("Proxy URL", proxy_url)
                    .keywords(["socks", "socks5", "network mode", "privacy"])
                    .layout(Axis::Vertical),
            );
        }
        if should_show_proxy_waku_disclaimer(self.draft.network.mode) {
            let dns_warning = proxy_url_resolves_dns_locally(&self.draft);
            network_group = network_group.footer(move |_window, _cx| {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(settings_warning_banner(
                        "wallet-settings-proxy-waku-warning",
                        PROXY_WAKU_DISCLAIMER,
                    ))
                    .when(dns_warning, |footer| {
                        footer.child(settings_warning_banner(
                            "wallet-settings-proxy-dns-warning",
                            PROXY_SOCKS5_DNS_WARNING,
                        ))
                    })
            });
        }
        let walletconnect_group = settings_group().title("WalletConnect").item(
            SettingItem::new("WalletConnect project ID", walletconnect_project_id)
                .keywords(["wallet connect", "reown", "dapp", "pairing"])
                .layout(Axis::Vertical),
        );
        let network_page = SettingPage::new("Network")
            .group(network_group)
            .group(walletconnect_group);

        let broadcasters_page = SettingPage::new("Broadcasters")
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
                            editor,
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
            );
        let storage_page = SettingPage::new("Storage")
            .group(
                settings_group().title("Data folder").item(
                    SettingItem::new(
                        "Location",
                        SettingField::<SharedString>::render(move |_options, window, cx| {
                            Self::render_data_folder_action(&data_folder_editor, window, cx)
                        }),
                    )
                    .keywords([
                        "data folder",
                        "database",
                        "db",
                        "path",
                        "disk space",
                        "size",
                        "file manager",
                    ])
                    .layout(Axis::Vertical),
                ),
            )
            .group(
                settings_group().title("Prover cache").item(
                    SettingItem::new(
                        "Build prover cache",
                        SettingField::<SharedString>::render(move |_options, _window, cx| {
                            Self::render_build_prover_cache_action(&prover_cache_editor, cx)
                        }),
                    )
                    .description("Prepares prover artifacts locally so transaction proof generation can start faster.")
                    .keywords(["proving", "circuits", "zkey", "download", "speed"])
                    .layout(Axis::Vertical),
                ),
            )
            .group(
                settings_group()
                    .title("Local caches")
                    .item(
                        SettingItem::new(
                            "Reset public sync caches",
                            SettingField::<SharedString>::render(move |_options, _window, cx| {
                                Self::render_local_poi_cache_reset_action(
                                    &poi_cache_reset_editor,
                                    cx,
                                )
                            }),
                        )
                        .description("Clears downloaded public sync data while preserving verified PPOI data used for wallet checks.")
                        .keywords(["clear", "txid", "artifact chunks", "resync", "local caches"])
                        .layout(Axis::Vertical),
                    )
                    .item(
                        SettingItem::new(
                            "Reset PPOI data",
                            SettingField::<SharedString>::render(move |_options, _window, cx| {
                                Self::render_poi_data_reset_action(&poi_data_reset_editor, cx)
                            }),
                        )
                        .description("Deletes verified PPOI data and downloaded PPOI chunks for all chains, then rebuilds from the current source.")
                        .keywords(["poi", "proof of innocence", "clear", "rebuild", "local caches"])
                        .layout(Axis::Vertical),
                    )
                    .item(
                        SettingItem::new(
                            "Reset local Merkle forest cache",
                            SettingField::<SharedString>::render(move |_options, _window, cx| {
                                Self::render_local_merkle_forest_cache_reset_action(
                                    &merkle_forest_cache_reset_editor,
                                    cx,
                                )
                            }),
                        )
                        .description("Clears local Merkle forest snapshots for all chains, then resyncs tree data before private actions are available again.")
                        .keywords(["merkle tree", "snapshots", "clear", "resync", "local caches"])
                        .layout(Axis::Vertical),
                    ),
            );
        let shell = div()
            .size_full()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap_3()
            .child(self.render_status_indicator(cx))
            .when_some(self.validation_error.clone(), |this, error| {
                this.child(settings_danger_banner(error.to_string()))
            })
            .when_some(self.render_status_message(cx), |this, status| {
                this.child(status)
            });
        if chain_editing {
            // An open chain owns the body and its own footer until it returns to the list.
            return shell.child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(self.chain_editor.clone()),
            );
        }
        shell
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(
                        ComponentSettings::new("wallet-settings-editor")
                            .when_some(open_page, |settings, page_ix| {
                                settings.default_selected_index(SelectIndex {
                                    page_ix,
                                    group_ix: None,
                                })
                            })
                            .sidebar_width(px(190.0))
                            .with_group_variant(GroupBoxVariant::Normal)
                            .page(general_page)
                            .page(security_page)
                            .page(network_page)
                            .page(chains_page)
                            .page(token_page)
                            .page(broadcasters_page)
                            .page(data_sources_page)
                            .page(storage_page),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("wallet-settings-discard", "Discard")
                            .disabled(!self.is_dirty())
                            .on_click(move |_event, _window, cx| {
                                discard_editor.update(cx, |editor, cx| {
                                    editor.discard_changes(cx);
                                });
                            }),
                    )
                    .child(
                        app_button("wallet-settings-reset", "Reset to defaults").on_click(
                            move |_event, _window, cx| {
                                reset_editor.update(cx, |editor, cx| {
                                    editor.reset_defaults(cx);
                                });
                            },
                        ),
                    )
                    .child(
                        app_button("wallet-settings-save", "Save")
                            .when(self.pre_start, ButtonVariants::primary)
                            .disabled(
                                !self.maintenance_controller.read(cx).is_idle() || !save_enabled,
                            )
                            .on_click(move |_event, window, cx| {
                                save_editor.update(cx, |editor, cx| {
                                    // Setup shows the saved values once Settings closes.
                                    if editor.save_draft(cx) && editor.pre_start {
                                        window.close_all_dialogs(cx);
                                    }
                                });
                            }),
                    )
                    .when(!self.pre_start, |footer| {
                        footer.child(
                            app_button("wallet-settings-apply-restart", "Apply")
                                .primary()
                                .disabled(
                                    !self.maintenance_controller.read(cx).is_idle()
                                        || !settings_restart_action_enabled(
                                            &self.saved,
                                            &self.draft,
                                            self.validation_error.is_some(),
                                            root_replacement_allowed,
                                        ),
                                )
                                .on_click(move |_event, window, cx| {
                                    apply_editor.update(cx, |editor, cx| {
                                        editor.apply_and_restart(window, cx);
                                    });
                                }),
                        )
                    }),
            )
    }
}

impl WalletSettingsEditor {
    fn device_auth_setting(&self, method: DeviceAuthMethod, cx: &App) -> Option<SettingItem> {
        let root = self.active_root.as_ref()?.upgrade()?;
        let checked = root.read(cx).device_auth_setting_state(method)?;
        let available = root.read(cx).device_auth_available(method);
        let description = if !available {
            match method {
                DeviceAuthMethod::TouchId => "Touch ID is unavailable right now. Check Touch ID in macOS System Settings.",
                DeviceAuthMethod::AppleWatch => "Apple Watch is unavailable right now. Make sure your watch is unlocked, on your wrist, and nearby.",
            }.to_owned()
        } else if root.read(cx).device_auth_status(method)
            == wallet_ops::vault::DeviceAuthStatus::NeedsReenrollment
        {
            format!(
                "Enter your vault password when prompted to restore {}. Changes apply immediately.",
                method.label()
            )
        } else {
            match method {
                DeviceAuthMethod::TouchId => "Use Touch ID instead of your vault password on this Mac. Changes apply immediately.",
                DeviceAuthMethod::AppleWatch => "Double-press your watch's side button for each approval. Never unlocks automatically. Changes apply immediately.",
            }.to_owned()
        };
        Some(
            SettingItem::new(
                format!("Use {}", method.label()),
                SettingField::<SharedString>::render(move |options, _window, _cx| {
                    let root = root.clone();
                    Switch::new(match method {
                        DeviceAuthMethod::TouchId => "wallet-settings-touch-id",
                        DeviceAuthMethod::AppleWatch => "wallet-settings-apple-watch",
                    })
                    .accessibility_label(format!("Use {}", method.label()))
                    .checked(checked)
                    // Revocation must remain available even while the device is not.
                    .disabled(!checked && !available)
                    .with_size(options.size())
                    .on_change(move |enabled, window, cx| {
                        root.update(cx, |root, cx| {
                            if *enabled {
                                root.open_enable_device_auth_dialog(method, window, cx);
                            } else {
                                root.disable_device_auth(method, window, cx);
                            }
                        });
                    })
                }),
            )
            .description(description)
            .keywords([
                "touch id",
                "apple watch",
                "biometric",
                "fingerprint",
                "device unlock",
                "password",
                "security",
            ]),
        )
    }
}
