use super::*;

/// Position of the Chains page in the `ComponentSettings` page list below.
const CHAINS_PAGE_INDEX: usize = 2;

const INDEXED_ARTIFACT_REPOSITORY_URL: &str = "https://github.com/triamazikamno/railgun-indexer/";

fn indexed_artifact_repository_help_item() -> SettingItem {
    SettingItem::render(move |_options, _window, _cx| {
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
    })
}

impl Render for WalletSettingsEditor {
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let editor = cx.entity();
        let root_replacement_allowed = self.root_replacement_is_allowed(cx);
        let chain_editing = self.chain_editor.read(cx).is_editing();
        let reopen_chains_page = std::mem::take(&mut self.reopen_chains_page);
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
                    SharedString::from("POI proxy"),
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
        let poi_reset_editor = editor.clone();
        let poi_cache_reset_editor = editor.clone();
        let poi_data_reset_editor = editor.clone();
        let merkle_forest_cache_reset_editor = editor.clone();
        let prover_cache_editor = editor.clone();
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
            max: f64::from(1024 * 1024 * 1024),
            step: f64::from(1024 * 1024),
        };
        let indexed_concurrency = Self::number_field(
            "indexed-artifact-concurrency",
            editor.clone(),
            indexed_concurrency_options,
            |settings| {
                settings
                    .indexed_artifacts
                    .concurrency
                    .unwrap_or(wallet_ops::settings::DEFAULT_INDEXED_ARTIFACT_CONCURRENCY)
                    as f64
            },
            |settings, value| settings.indexed_artifacts.concurrency = Some(value as usize),
        );
        let indexed_byte_budget = Self::number_field(
            "indexed-artifact-byte-budget",
            editor.clone(),
            indexed_byte_budget_options,
            |settings| {
                settings
                    .indexed_artifacts
                    .max_in_flight_bytes
                    .unwrap_or(wallet_ops::settings::DEFAULT_INDEXED_ARTIFACT_MAX_IN_FLIGHT_BYTES)
                    as f64
            },
            |settings, value| settings.indexed_artifacts.max_in_flight_bytes = Some(value as u64),
        );
        let indexed_reset_editor = editor.clone();
        let waku_number_options = NumberFieldOptions {
            min: 0.0,
            max: f64::from(u32::MAX),
            step: 1.0,
        };
        let positive_number_options = NumberFieldOptions {
            min: 1.0,
            max: 86_400.0,
            step: 1.0,
        };
        let waku_cluster = Self::number_field(
            "waku-cluster-id",
            editor.clone(),
            waku_number_options.clone(),
            |settings| f64::from(settings.waku.cluster_id),
            |settings, value| settings.waku.cluster_id = value as u32,
        );
        let waku_shard = Self::number_field(
            "waku-shard-id",
            editor.clone(),
            waku_number_options,
            |settings| f64::from(settings.waku.shard_id),
            |settings, value| settings.waku.shard_id = value as u32,
        );
        let waku_max_peers = Self::number_field(
            "waku-max-peers",
            editor.clone(),
            positive_number_options.clone(),
            |settings| settings.waku.max_peers as f64,
            |settings, value| settings.waku.max_peers = value as usize,
        );
        let waku_timeout = Self::number_field(
            "waku-peer-timeout-seconds",
            editor.clone(),
            positive_number_options.clone(),
            |settings| settings.waku.peer_connection_timeout_secs as f64,
            |settings, value| settings.waku.peer_connection_timeout_secs = value as u64,
        );
        let broadcaster_timeout = Self::number_field(
            "broadcaster-response-timeout-seconds",
            editor.clone(),
            positive_number_options.clone(),
            |settings| settings.broadcaster.response_timeout_secs as f64,
            |settings, value| settings.broadcaster.response_timeout_secs = value as u64,
        );
        let broadcaster_republish_interval = Self::number_field(
            "broadcaster-republish-interval-seconds",
            editor.clone(),
            positive_number_options,
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
        let waku_direct_peers = display_waku_direct_peers(&self.draft);
        let waku_direct_peers_editor = editor.clone();
        let waku_doh_fallback_kind = SettingsUrlListKind::WakuDohFallback;
        let waku_doh_fallback_endpoints = waku_doh_fallback_kind.endpoints(&self.draft);
        let waku_doh_fallback_editor = editor.clone();

        let indexed_artifact_status = format!(
            "Current index source priority order: {}",
            indexed_artifact_source_status_message(&self.draft)
        );
        let mut indexed_artifact_group = settings_group()
            .item(settings_section_header("Indexed artifacts"))
            .item(SettingItem::render(move |_options, _window, _cx| {
                settings_info_banner(&indexed_artifact_status)
            }))
            .item(SettingItem::new(
                "Indexed artifact source",
                indexed_source_mode,
            ))
            .item(indexed_artifact_repository_help_item());
        if should_show_indexed_artifact_custom_settings(self.draft.indexed_artifacts.source_mode) {
            indexed_artifact_group = indexed_artifact_group
                .item(
                    SettingItem::new("Publisher public key", indexed_publisher)
                        .layout(Axis::Vertical),
                )
                .item(SettingItem::new("IPNS name", indexed_ipns).layout(Axis::Vertical))
                .item(SettingItem::new("Chunk concurrency", indexed_concurrency))
                .item(SettingItem::new(
                    "In-flight byte budget",
                    indexed_byte_budget,
                ))
                .item(SettingItem::new(
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
                ));
        }

        let poi_gateway_kind = SettingsUrlListKind::PoiGateway;
        let poi_gateway_endpoints = poi_gateway_kind.endpoints(&self.draft);
        let poi_gateway_editor = editor.clone();
        let poi_gateway_group = settings_group()
            .item(settings_section_header("Artifact gateways"))
            .item(Self::settings_url_list_item(
                "Artifact gateway URLs",
                poi_gateway_editor,
                poi_gateway_kind,
                poi_gateway_endpoints,
            ));

        let chain_editor = self.chain_editor.clone();
        let chains_page = SettingPage::new("Chains")
            .group(settings_group().item(SettingItem::render(move |_, _, _| {
                div().w_full().child(chain_editor.clone())
            })))
            .group(indexed_artifact_group);

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
                .layout(Axis::Vertical),
            ),
        );

        let save_editor = editor.clone();
        let discard_editor = editor.clone();
        let reset_editor = editor.clone();
        let apply_editor = editor.clone();
        let security_page = SettingPage::new("Security").group(
            settings_group().item(
                SettingItem::new("Auto-lock vault", auto_lock_timeout)
                    .description("Lock the vault after this long without wallet activity."),
            ),
        );
        let mut privacy_group = settings_group()
            .item(SettingItem::new("Network mode", network_mode))
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
                ),
            );
        if should_show_proxy_waku_disclaimer(self.draft.network.mode) {
            privacy_group = privacy_group.item(SettingItem::render(|_options, _window, _cx| {
                settings_warning_banner(PROXY_WAKU_DISCLAIMER)
            }));
        }
        if should_show_proxy_url_setting(self.draft.network.mode) {
            privacy_group =
                privacy_group.item(SettingItem::new("Proxy URL", proxy_url).layout(Axis::Vertical));
        }
        let privacy_page = SettingPage::new("Privacy")
            .group(privacy_group)
            .group(
                settings_group()
                    .item(settings_section_header("POI"))
                    .item(SettingItem::new("POI source", poi_source).description("'Indexed artifacts' downloads snapshots containing POI data from IPFS and uses the POI RPC URL only to live-tail recent public POI events. 'POI proxy' mode is less private: the POI RPC receives requests containing blind commitment hashes associated with UTXOs you are receiving or preparing to spend. Use POI proxy mode only if you trust the POI RPC operator."))
                    .item(SettingItem::new("POI RPC URL", poi_rpc_url).description("Used for indexed-artifact live tailing and for direct POI status/proof requests when POI proxy mode is selected.").layout(Axis::Vertical))
                    .item(
                        SettingItem::new("Publisher public key", poi_publisher)
                            .layout(Axis::Vertical),
                    )
                    .item(
                        SettingItem::new("IPNS name", poi_ipns)
                            .layout(Axis::Vertical),
                    )
                    .item(SettingItem::new(
                        "Reset POI artifact defaults",
                        SettingField::<SharedString>::render(move |_options, _window, _cx| {
                            let reset_editor = poi_reset_editor.clone();
                            app_button("wallet-settings-poi-official-preset", "Reset to default")
                                .on_click(move |_event, _window, cx| {
                                    reset_editor.update(cx, |editor, cx| {
                                        editor.draft.poi.reset_artifact_to_official_preset();
                                        editor.programmatic_draft_changed(cx);
                                    });
                                })
                        }),
                    )),
            )
            .group(poi_gateway_group);
        let public_broadcasters_page = SettingPage::new("Public Broadcasters")
            .group(
                settings_group()
                    .item(Self::broadcaster_anchor_range_item(editor.clone()))
                    .item(Self::settings_switch_item(
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
                    ))
                    .item(SettingItem::new(
                        "Response timeout seconds",
                        broadcaster_timeout,
                    ))
                    .item(SettingItem::new(
                        "Republish interval seconds",
                        broadcaster_republish_interval,
                    )),
            )
            .group(
                settings_group()
                    .item(settings_section_header("Waku connectivity"))
                    .item(SettingItem::new("Cluster ID", waku_cluster))
                    .item(SettingItem::new("Shard ID", waku_shard))
                    .item(Self::settings_url_list_item(
                        "DNS ENR trees",
                        waku_dns_enr_editor,
                        waku_dns_enr_kind,
                        waku_dns_enr_trees,
                    ))
                    .item(Self::waku_direct_peer_list_item(
                        waku_direct_peers_editor,
                        waku_direct_peers,
                    ))
                    .item(SettingItem::new("DoH endpoint", waku_doh).layout(Axis::Vertical))
                    .item(Self::settings_url_list_item(
                        "DoH fallback endpoints",
                        waku_doh_fallback_editor,
                        waku_doh_fallback_kind,
                        waku_doh_fallback_endpoints,
                    ))
                    .item(SettingItem::new("Max peers", waku_max_peers))
                    .item(SettingItem::new("Peer timeout seconds", waku_timeout))
                    .item(SettingItem::new("nwaku REST URL", waku_nwaku).layout(Axis::Vertical)),
            );
        let walletconnect_page =
            SettingPage::new("WalletConnect").group(settings_group().item(
                SettingItem::new("Project ID", walletconnect_project_id).layout(Axis::Vertical),
            ));
        let maintenance_page = SettingPage::new("Maintenance")
            .group(
                settings_group()
                    .item(settings_section_header("Prover cache"))
                    .item(
                        SettingItem::new(
                            "Build prover cache",
                            SettingField::<SharedString>::render(move |_options, _window, cx| {
                                Self::render_build_prover_cache_action(&prover_cache_editor, cx)
                            }),
                        )
                        .description("Prepares prover artifacts locally so transaction proof generation can start faster.")
                        .layout(Axis::Vertical),
                    ),
            )
            .group(
                settings_group()
                    .item(settings_section_header("Local caches"))
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
                            .when(reopen_chains_page, |settings| {
                                settings.default_selected_index(SelectIndex {
                                    page_ix: CHAINS_PAGE_INDEX,
                                    group_ix: None,
                                })
                            })
                            .sidebar_width(px(190.0))
                            .with_group_variant(GroupBoxVariant::Normal)
                            .page(security_page)
                            .page(privacy_page)
                            .page(chains_page)
                            .page(token_page)
                            .page(public_broadcasters_page)
                            .page(walletconnect_page)
                            .page(maintenance_page),
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
                            .disabled(
                                !self.maintenance_controller.read(cx).is_idle()
                                    || !settings_save_action_enabled(
                                        &self.saved,
                                        &self.draft,
                                        self.validation_error.is_some(),
                                    ),
                            )
                            .on_click(move |_event, _window, cx| {
                                save_editor.update(cx, |editor, cx| {
                                    editor.save_draft(cx);
                                });
                            }),
                    )
                    .child(
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
                    ),
            )
    }
}
