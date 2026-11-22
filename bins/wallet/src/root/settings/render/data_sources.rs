use super::*;

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

impl WalletSettingsEditor {
    pub(super) fn data_sources_page(&self, editor: &Entity<Self>) -> SettingPage {
        let indexed_artifact_group = self.indexed_artifact_group(editor);
        let poi_group = self.poi_group(editor);
        let poi_gateway_group = self.poi_gateway_group(editor);
        SettingPage::new("Data sources")
            .group(indexed_artifact_group)
            .group(poi_group)
            .group(poi_gateway_group)
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    fn indexed_artifact_group(&self, editor: &Entity<Self>) -> SettingGroup {
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
                .item(self.indexed_artifact_amount_statistics_item(editor))
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
        indexed_artifact_group
    }

    /// The custom source's amount statistics IPNS name, described by its help or by why it is
    /// invalid.
    fn indexed_artifact_amount_statistics_item(&self, editor: &Entity<Self>) -> SettingItem {
        let ipns_name = Self::shared_string_field(
            "indexed-artifact-amount-statistics-ipns-name",
            editor.clone(),
            |settings| {
                settings
                    .indexed_artifacts
                    .amount_statistics_ipns_name
                    .clone()
                    .unwrap_or_default()
            },
            |settings, value| {
                settings.indexed_artifacts.amount_statistics_ipns_name = non_empty_setting(&value);
            },
        );
        SettingItem::new("Amount statistics IPNS name", ipns_name)
            .description(indexed_artifact_amount_statistics_help(
                &self.draft,
                self.validation_error.as_deref(),
            ))
            .keywords([
                "indexed artifacts",
                "amount statistics",
                "privacy hints",
                "ipns",
                "custom",
            ])
            .layout(Axis::Vertical)
    }

    fn poi_group(&self, editor: &Entity<Self>) -> SettingGroup {
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
        poi_group
    }

    fn poi_gateway_group(&self, editor: &Entity<Self>) -> SettingGroup {
        let poi_gateway_kind = SettingsUrlListKind::PoiGateway;
        let poi_gateway_endpoints = poi_gateway_kind.endpoints(&self.draft);
        let poi_gateway_editor = editor.clone();
        settings_group().title("Artifact gateways").item(
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
        )
    }
}
