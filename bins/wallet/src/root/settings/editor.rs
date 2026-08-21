use super::*;

const WALLET_DELETION_APPLY_STATUS: &str =
    "Wait for wallet deletion to finish before applying settings";

impl WalletSettingsEditor {
    pub(in crate::root) fn new(
        vault_store: Arc<DesktopVaultStore>,
        runtime: Handle,
        settings: WalletSettings,
        maintenance_controller: Entity<WalletMaintenanceController>,
        startup_root: Option<WeakEntity<WalletStartupRoot>>,
        active_root: Option<WeakEntity<WalletRoot>>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        cx.observe(&maintenance_controller, |_editor, _controller, cx| {
            cx.notify();
        })
        .detach();
        let mut editor = Self {
            vault_store,
            runtime,
            saved: settings.clone(),
            draft: settings,
            field_sync_revision: 0,
            validation_error: None,
            status: None,
            cache_building: false,
            cache_build_progress: None,
            maintenance_controller,
            startup_root,
            active_root,
        };
        editor.refresh_validation();
        editor
    }

    pub(in crate::root) fn refresh_validation(&mut self) {
        self.validation_error = self
            .draft
            .validate()
            .err()
            .map(|error| Arc::from(error.to_string()));
    }

    pub(in crate::root) fn draft_changed(&mut self, cx: &mut Context<'_, Self>) {
        self.status = None;
        self.refresh_validation();
        cx.notify();
    }

    const fn sync_fields_from_draft(&mut self) {
        self.field_sync_revision = self.field_sync_revision.wrapping_add(1);
    }

    pub(in crate::root) fn programmatic_draft_changed(&mut self, cx: &mut Context<'_, Self>) {
        self.sync_fields_from_draft();
        self.draft_changed(cx);
    }

    pub(in crate::root) fn is_dirty(&self) -> bool {
        self.draft != self.saved
    }

    pub(in crate::root) fn root_replacement_is_allowed(&self, cx: &App) -> bool {
        if let Some(startup_root) = self.startup_root.as_ref() {
            return startup_root
                .read_with(cx, WalletStartupRoot::root_replacement_is_allowed)
                .unwrap_or(false);
        }
        self.active_root.as_ref().is_none_or(|root| {
            root.read_with(cx, |root, _cx| root.root_replacement_is_allowed())
                .unwrap_or(false)
        })
    }

    pub(in crate::root) fn render_status_indicator(&self, cx: &App) -> gpui::Div {
        let (label, color) = if !self.maintenance_controller.read(cx).is_idle() {
            ("Maintenance", theme::INFO)
        } else if !self.root_replacement_is_allowed(cx) {
            ("Wallet deletion", theme::INFO)
        } else if self.validation_error.is_some() {
            ("Invalid", theme::DANGER)
        } else if !self.is_dirty() {
            ("Saved", theme::SUCCESS)
        } else {
            match classify_settings_apply_mode(&self.saved, &self.draft) {
                SettingsApplyMode::NetworkingRestart => ("Restart required", theme::WARNING),
                SettingsApplyMode::NewRequests | SettingsApplyMode::FutureSessions => {
                    ("Unsaved", theme::WARNING)
                }
                SettingsApplyMode::Clean => ("Saved", theme::SUCCESS),
            }
        };

        div()
            .w_full()
            .flex()
            .justify_end()
            .items_center()
            .gap_2()
            .text_size(px(12.0))
            .text_color(rgb(theme::TEXT_MUTED))
            .child(div().size(px(7.0)).rounded_full().bg(rgb(color)))
            .child(label)
    }

    pub(in crate::root) fn render_status_message(&self, cx: &App) -> Option<gpui::Div> {
        let controller = self.maintenance_controller.read(cx);
        let maintenance_status = controller.status();
        if controller.is_idle() && !self.root_replacement_is_allowed(cx) {
            return Some(settings_info_banner(WALLET_DELETION_APPLY_STATUS));
        }
        let status = if controller.is_idle() {
            self.status.as_ref().or(maintenance_status.as_ref())?
        } else {
            maintenance_status.as_ref().or(self.status.as_ref())?
        };
        if status.as_ref() == "Settings saved" {
            return None;
        }
        Some(settings_info_banner(status.to_string()))
    }

    pub(in crate::root) fn clear_transient_status(&mut self, cx: &mut Context<'_, Self>) {
        if self.cache_building || !self.maintenance_controller.read(cx).is_idle() {
            return;
        }
        self.maintenance_controller.update(cx, |controller, cx| {
            controller.clear_status(cx);
        });
        if self.status.take().is_some() {
            cx.notify();
        }
    }

    pub(in crate::root) fn save_draft(&mut self, cx: &mut Context<'_, Self>) -> bool {
        if !self.maintenance_controller.read(cx).is_idle() {
            self.status = Some(Arc::from(
                "Wait for the active cache reset before changing settings",
            ));
            cx.notify();
            return false;
        }
        if classify_settings_apply_mode(&self.saved, &self.draft)
            == SettingsApplyMode::NetworkingRestart
        {
            self.status = Some(Arc::from(
                "Use Apply and restart networking for networking changes",
            ));
            cx.notify();
            return false;
        }
        self.persist_draft(cx)
    }

    pub(in crate::root) fn persist_draft(&mut self, cx: &mut Context<'_, Self>) -> bool {
        if !self.maintenance_controller.read(cx).is_idle() {
            self.status = Some(Arc::from(
                "Wait for the active cache reset before changing settings",
            ));
            cx.notify();
            return false;
        }
        self.refresh_validation();
        if self.validation_error.is_some() {
            self.status = Some(Arc::from("Fix validation errors before saving settings"));
            cx.notify();
            return false;
        }
        let apply_mode = classify_settings_apply_mode(&self.saved, &self.draft);
        let db = self.vault_store.db();
        match save_wallet_settings(db.as_ref(), &self.draft) {
            Ok(()) => {
                self.saved = self.draft.clone();
                self.apply_saved_settings_to_active_root(apply_mode, cx);
                self.status = Some(Arc::from("Settings saved"));
                cx.notify();
                true
            }
            Err(error) => {
                self.status = Some(Arc::from(format!("Failed to save settings: {error}")));
                cx.notify();
                false
            }
        }
    }

    pub(in crate::root) fn discard_changes(&mut self, cx: &mut Context<'_, Self>) {
        self.draft = settings_draft_after_discard(&self.saved);
        self.sync_fields_from_draft();
        self.refresh_validation();
        cx.notify();
    }

    pub(in crate::root) fn reset_defaults(&mut self, cx: &mut Context<'_, Self>) {
        self.draft = WalletSettings::default();
        self.sync_fields_from_draft();
        self.refresh_validation();
        cx.notify();
    }

    pub(in crate::root) fn open_local_poi_cache_reset_dialog(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.maintenance_controller.read(cx).is_idle() {
            return;
        }
        let editor = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(520.0));
        let dialog_max_height = dialog_max_height(window);
        let content_max_height = dialog_content_max_height(window);
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let confirm_editor = editor.clone();
            confirmation_dialog(
                dialog,
                ConfirmationDialogProps::danger(
                    "Reset public sync caches",
                    "Clears downloaded artifact chunks and TXID public cache data. Active wallet sessions also discard public scan coverage and resync.",
                    Some("Verified PPOI data, wallet data, keys, and balances are preserved."),
                    "Reset public cache",
                ),
                dialog_width,
                dialog_max_height,
                content_max_height,
            )
            .on_ok(move |_event, _window, cx| {
                confirm_editor.update(cx, |editor, cx| {
                    editor.confirm_local_poi_cache_reset(cx);
                });
                true
            })
        });
    }

    pub(in crate::root) fn confirm_local_poi_cache_reset(&mut self, cx: &mut Context<'_, Self>) {
        self.status = None;
        let controller = self.maintenance_controller.clone();
        let vault_store = Arc::clone(&self.vault_store);
        controller.update(cx, |controller, cx| {
            controller.start_public_reset(vault_store.as_ref(), cx);
        });
    }

    pub(in crate::root) fn open_poi_data_reset_dialog(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.maintenance_controller.read(cx).is_idle() {
            return;
        }
        let editor = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(520.0));
        let dialog_max_height = dialog_max_height(window);
        let content_max_height = dialog_content_max_height(window);
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let confirm_editor = editor.clone();
            confirmation_dialog(
                dialog,
                ConfirmationDialogProps::danger(
                    "Reset PPOI data",
                    "Deletes verified PPOI data and downloaded PPOI chunks for all chains. PPOI checks will be unavailable until current data downloads and verifies again.",
                    Some("Wallet data, keys, balances, settings, and publisher rollback protection are preserved."),
                    "Reset PPOI data",
                ),
                dialog_width,
                dialog_max_height,
                content_max_height,
            )
            .on_ok(move |_event, _window, cx| {
                confirm_editor.update(cx, |editor, cx| {
                    editor.confirm_poi_data_reset(cx);
                });
                true
            })
        });
    }

    pub(in crate::root) fn confirm_poi_data_reset(&mut self, cx: &mut Context<'_, Self>) {
        self.status = None;
        let controller = self.maintenance_controller.clone();
        let vault_store = Arc::clone(&self.vault_store);
        controller.update(cx, |controller, cx| {
            controller.start_poi_reset(vault_store.as_ref(), cx);
        });
    }

    pub(in crate::root) fn open_local_merkle_forest_cache_reset_dialog(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.maintenance_controller.read(cx).is_idle() {
            return;
        }
        let editor = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(560.0));
        let dialog_max_height = dialog_max_height(window);
        let content_max_height = dialog_content_max_height(window);
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let confirm_editor = editor.clone();
            confirmation_dialog(
                dialog,
                ConfirmationDialogProps::danger(
                    "Reset local Merkle forest cache",
                    "Clears local Merkle forest snapshots for all chains. The wallet will resync tree data before private actions are available again.",
                    Some("Wallet data and keys are not deleted."),
                    "Reset Merkle cache",
                ),
                dialog_width,
                dialog_max_height,
                content_max_height,
            )
            .on_ok(move |_event, _window, cx| {
                confirm_editor.update(cx, |editor, cx| {
                    editor.confirm_local_merkle_forest_cache_reset(cx);
                });
                true
            })
        });
    }

    pub(in crate::root) fn confirm_local_merkle_forest_cache_reset(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        self.status = None;
        let controller = self.maintenance_controller.clone();
        let vault_store = Arc::clone(&self.vault_store);
        controller.update(cx, |controller, cx| {
            controller.start_merkle_reset(vault_store.as_ref(), cx);
        });
    }

    pub(in crate::root) fn render_build_prover_cache_action(
        editor: &Entity<Self>,
        cx: &App,
    ) -> gpui::Div {
        let (building, invalid) = {
            let state = editor.read(cx);
            (state.cache_building, state.validation_error.is_some())
        };
        let label = if building {
            "Building..."
        } else {
            "Build prover cache"
        };
        let build_editor = editor.clone();
        div().flex().items_center().child(
            app_button("wallet-settings-build-cache", label)
                .disabled(building || invalid)
                .on_click(move |_event, _window, cx| {
                    build_editor.update(cx, |editor, cx| {
                        editor.build_prover_cache(cx);
                    });
                }),
        )
    }

    pub(in crate::root) fn render_local_poi_cache_reset_action(
        editor: &Entity<Self>,
        cx: &App,
    ) -> gpui::Div {
        let controller = editor.read(cx).maintenance_controller.clone();
        let reset = controller.read(cx).reset();
        let resetting = reset == WalletMaintenanceReset::Public;
        let reset_editor = editor.clone();
        let label = if resetting { "Resetting..." } else { "Reset" };
        div().flex().items_center().child(
            app_button("wallet-settings-poi-cache-reset", label)
                .danger()
                .outline()
                .disabled(reset != WalletMaintenanceReset::Idle)
                .on_click(move |_event, window, cx| {
                    reset_editor.update(cx, |editor, cx| {
                        editor.open_local_poi_cache_reset_dialog(window, cx);
                    });
                }),
        )
    }

    pub(in crate::root) fn render_poi_data_reset_action(
        editor: &Entity<Self>,
        cx: &App,
    ) -> gpui::Div {
        let controller = editor.read(cx).maintenance_controller.clone();
        let reset = controller.read(cx).reset();
        let resetting = reset == WalletMaintenanceReset::Poi;
        let reset_editor = editor.clone();
        let label = if resetting { "Resetting..." } else { "Reset" };
        div().flex().items_center().child(
            app_button("wallet-settings-poi-data-reset", label)
                .danger()
                .outline()
                .disabled(reset != WalletMaintenanceReset::Idle)
                .on_click(move |_event, window, cx| {
                    reset_editor.update(cx, |editor, cx| {
                        editor.open_poi_data_reset_dialog(window, cx);
                    });
                }),
        )
    }

    pub(in crate::root) fn render_local_merkle_forest_cache_reset_action(
        editor: &Entity<Self>,
        cx: &App,
    ) -> gpui::Div {
        let controller = editor.read(cx).maintenance_controller.clone();
        let reset = controller.read(cx).reset();
        let resetting = reset == WalletMaintenanceReset::Merkle;
        let reset_editor = editor.clone();
        let label = if resetting { "Resetting..." } else { "Reset" };
        div().flex().items_center().child(
            app_button("wallet-settings-merkle-forest-reset", label)
                .danger()
                .outline()
                .disabled(reset != WalletMaintenanceReset::Idle)
                .on_click(move |_event, window, cx| {
                    reset_editor.update(cx, |editor, cx| {
                        editor.open_local_merkle_forest_cache_reset_dialog(window, cx);
                    });
                }),
        )
    }

    pub(in crate::root) fn apply_and_restart(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.maintenance_controller.read(cx).is_idle() {
            self.status = Some(Arc::from(
                "Wait for the active cache reset before changing settings",
            ));
            cx.notify();
            return;
        }
        if !self.root_replacement_is_allowed(cx) {
            self.status = Some(Arc::from(WALLET_DELETION_APPLY_STATUS));
            cx.notify();
            return;
        }
        let reusable_http = if settings_restart_reuses_active_network(&self.saved, &self.draft) {
            self.active_root.as_ref().and_then(|root| {
                root.update(cx, |root, _cx| root.reusable_network_context())
                    .ok()
            })
        } else {
            None
        };
        if !self.persist_draft(cx) {
            return;
        }
        window.close_all_dialogs(cx);
        if let Some(root) = self.startup_root.clone() {
            let _ = root.update(cx, |root, cx| {
                root.retry_startup_with_network_context(reusable_http, window, cx);
            });
        }
    }

    pub(in crate::root) fn apply_saved_settings_to_active_root(
        &self,
        apply_mode: SettingsApplyMode,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(root) = self.active_root.clone() else {
            return;
        };
        let settings = self.saved.clone();
        // Request settings may consult the editor through WalletConnect; release this update first.
        cx.defer(move |cx| {
            let _ = root.update(cx, |root, cx| {
                root.apply_saved_auto_lock_policy(&settings);
                if apply_mode == SettingsApplyMode::NewRequests {
                    root.apply_saved_request_settings(&settings, cx);
                }
            });
        });
    }

    pub(in crate::root) fn prepare_prover_cache_build(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<PreparedProverCacheBuild, Arc<str>> {
        if self.cache_building || self.cache_build_progress.is_some() {
            return Err(Arc::from("Prover cache build is already running"));
        }
        self.refresh_validation();
        if self.validation_error.is_some() {
            return Err(Arc::from(
                "Fix validation errors before building prover cache",
            ));
        }
        let proxy = self
            .draft
            .network
            .proxy_url
            .as_deref()
            .map(reqwest::Url::parse)
            .transpose()
            .map_err(|error| Arc::from(format!("Invalid proxy URL: {error}")))?;
        let db = self.vault_store.db();
        let db_path = db.root_dir().to_path_buf();
        let network_mode = self.draft.wallet_network_mode();
        let reuse_active_network = settings_restart_reuses_active_network(&self.saved, &self.draft);
        cx.notify();
        Ok(PreparedProverCacheBuild {
            params: ProverCacheBuildParams {
                db,
                db_path,
                network_mode,
                proxy,
                reusable_http: None,
            },
            reuse_active_network,
        })
    }

    pub(in crate::root) fn build_prover_cache(&mut self, cx: &mut Context<'_, Self>) {
        let prepared = match self.prepare_prover_cache_build(cx) {
            Ok(prepared) => prepared,
            Err(message) => {
                self.status = Some(message);
                cx.notify();
                return;
            }
        };
        let initial_progress = ProverCacheBuildProgress::preparing();
        if let Some(root) = self.active_root.as_ref() {
            let editor = cx.entity();
            let params = prepared.params.clone();
            let reuse_active_network = prepared.reuse_active_network;
            let start = root.update(cx, |root, cx| {
                let mut params = params;
                let reusable_http = if reuse_active_network {
                    Some(root.reusable_network_context())
                } else {
                    None
                };
                params.reusable_http = reusable_http;
                root.start_prover_cache_build_from_settings(editor, params, cx)
            });
            match start {
                Ok(Ok(())) => {
                    self.mark_cache_build_started(initial_progress, cx);
                    return;
                }
                Ok(Err(message)) => {
                    self.status = Some(message);
                    cx.notify();
                    return;
                }
                Err(error) => {
                    tracing::debug!(%error, "falling back to local prover cache build task");
                }
            }
        }

        self.start_local_prover_cache_build(prepared.params, initial_progress, cx);
    }

    pub(in crate::root) fn mark_cache_build_started(
        &mut self,
        initial_progress: ProverCacheBuildProgress,
        cx: &mut Context<'_, Self>,
    ) {
        self.cache_building = true;
        self.status = Some(Arc::from("Building prover cache..."));
        self.cache_build_progress = Some(initial_progress);
        cx.notify();
    }

    pub(in crate::root) fn start_local_prover_cache_build(
        &mut self,
        params: ProverCacheBuildParams,
        initial_progress: ProverCacheBuildProgress,
        cx: &mut Context<'_, Self>,
    ) {
        let ProverCacheBuildParams {
            db,
            db_path,
            network_mode,
            proxy,
            reusable_http,
        } = params;
        let session = match begin_prover_cache_build(&db_path) {
            Ok(session) => session,
            Err(error) => {
                self.status = Some(Arc::from(error.to_string()));
                cx.notify();
                return;
            }
        };
        self.mark_cache_build_started(initial_progress.clone(), cx);
        let (progress_tx, mut progress_rx) = watch::channel(initial_progress);
        let join = self.runtime.spawn(async move {
            let http = if let Some(http) = reusable_http {
                http
            } else {
                build_wallet_network_context(WalletNetworkConfig {
                    network_mode: Some(network_mode),
                    proxy: proxy.as_ref(),
                    data_dir: &db_path,
                })
                .await?
            };
            build_cache_with_context_and_progress_with_session(
                db,
                &http,
                session,
                move |progress| {
                    let _ = progress_tx.send(progress);
                },
            )
            .await
        });
        cx.spawn(async move |this, cx| {
            tokio::pin!(join);
            let mut progress_open = true;
            loop {
                tokio::select! {
                    result = &mut join => {
                        let _ = this.update(cx, |editor, cx| {
                            editor.cache_building = false;
                            editor.cache_build_progress = None;
                            editor.status = Some(Arc::from(match result {
                                Ok(Ok(report)) => format!(
                                    "Prover cache build complete: {}/{} variants succeeded",
                                    report.succeeded_variants, report.total_variants
                                ),
                                Ok(Err(error)) => format!("Prover cache build failed: {error}"),
                                Err(error) => format!("Prover cache task failed: {error}"),
                            }));
                            cx.notify();
                        });
                        break;
                    }
                    changed = progress_rx.changed(), if progress_open => {
                        if changed.is_err() {
                            progress_open = false;
                            continue;
                        }
                        let progress = progress_rx.borrow().clone();
                        let editor_progress = progress.clone();
                        let _ = this.update(cx, |editor, cx| {
                            editor.cache_build_progress = Some(editor_progress);
                            cx.notify();
                        });
                    }
                }
            }
        })
        .detach();
        cx.notify();
    }

    pub(in crate::root) fn shared_string_field(
        field_id: impl Into<String>,
        editor: Entity<Self>,
        get: impl Fn(&WalletSettings) -> String + 'static,
        set: impl Fn(&mut WalletSettings, String) + 'static,
    ) -> SettingField<SharedString> {
        let field_id = field_id.into();
        let get = Rc::new(get);
        let set = Rc::new(set);
        SettingField::render(move |options, window, cx| {
            let (value, revision) = {
                let editor = editor.read(cx);
                (
                    SharedString::from(get(&editor.draft)),
                    editor.field_sync_revision,
                )
            };
            let state = window.use_keyed_state(
                SharedString::from(format!("wallet-settings-string-{field_id}")),
                cx,
                {
                    let value = value.clone();
                    let set = set.clone();
                    let editor = editor.clone();
                    move |window, cx| {
                        let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
                        let subscription = cx.subscribe_in(&input, window, {
                            let set = set.clone();
                            let editor = editor.clone();
                            move |state: &mut SyncedStringFieldState,
                                  input,
                                  event: &InputEvent,
                                  _window,
                                  cx| {
                                if !matches!(event, InputEvent::Change) {
                                    return;
                                }
                                if state.ignore_next_change {
                                    state.ignore_next_change = false;
                                    return;
                                }
                                let value = input.read(cx).value().to_string();
                                editor.update(cx, |editor, cx| {
                                    set(&mut editor.draft, value);
                                    editor.draft_changed(cx);
                                });
                            }
                        });
                        SyncedStringFieldState {
                            input,
                            synced_revision: revision,
                            ignore_next_change: false,
                            _subscription: subscription,
                        }
                    }
                },
            );

            state.update(cx, |state, cx| {
                if state.synced_revision == revision {
                    return;
                }
                state.synced_revision = revision;
                if state.input.read(cx).value().as_ref() == value.as_ref() {
                    return;
                }
                state.ignore_next_change = true;
                state
                    .input
                    .update(cx, |input, cx| input.set_value(value.clone(), window, cx));
            });

            let input = state.read(cx).input.clone();
            settings_text_input(&input)
                .with_size(options.size)
                .map(|this| {
                    if matches!(options.layout, Axis::Horizontal) {
                        this.w_64()
                    } else {
                        this.w_full()
                    }
                })
        })
    }

    pub(in crate::root) fn settings_switch_item(
        row_id: impl Into<String>,
        label: impl Into<String>,
        editor: Entity<Self>,
        _icon_chain_id: Option<u64>,
        get: impl Fn(&WalletSettings) -> bool + 'static,
        set: impl Fn(&mut WalletSettings, bool) + 'static,
    ) -> SettingItem {
        let row_id = row_id.into();
        let label = label.into();
        let get = Rc::new(get);
        let set = Rc::new(set);
        SettingItem::new(
            label,
            SettingField::<SharedString>::render(move |options, _window, cx| {
                let checked = get(&editor.read(cx).draft);
                let set_editor = editor.clone();
                let set_from_switch = set.clone();
                div()
                    .id(SharedString::from(row_id.clone()))
                    .flex()
                    .items_center()
                    .child(
                        Switch::new(SharedString::from(format!("{row_id}-switch")))
                            .checked(checked)
                            .with_size(options.size)
                            .on_click(move |enabled, _window, cx| {
                                set_editor.update(cx, |editor, cx| {
                                    set_from_switch(&mut editor.draft, *enabled);
                                    editor.draft_changed(cx);
                                });
                            }),
                    )
            }),
        )
    }

    pub(in crate::root) fn chain_enabled_item(editor: Entity<Self>, chain_id: u64) -> SettingItem {
        let label = chain_name(chain_id).map_or_else(|| chain_id.to_string(), ToString::to_string);
        Self::settings_switch_item(
            format!("wallet-settings-chain-row-{chain_id}"),
            label,
            editor,
            Some(chain_id),
            move |settings| {
                settings
                    .chains
                    .per_chain
                    .get(&chain_id)
                    .is_none_or(|chain| chain.enabled)
            },
            move |settings, enabled| {
                settings
                    .chains
                    .per_chain
                    .entry(chain_id)
                    .or_default()
                    .enabled = enabled;
            },
        )
    }

    pub(in crate::root) fn broadcaster_anchor_range_item(editor: Entity<Self>) -> SettingItem {
        SettingItem::new(
            "Accepted fee range",
            SettingField::<SharedString>::render(move |_options, window, cx| {
                let (min_bps, max_bps, revision) = {
                    let editor = editor.read(cx);
                    let (min_bps, max_bps) = broadcaster_anchor_bps_range(&editor.draft);
                    (min_bps, max_bps, editor.field_sync_revision)
                };
                let state = window.use_keyed_state(
                    SharedString::from("wallet-settings-broadcaster-anchor-range-slider"),
                    cx,
                    {
                        let editor = editor.clone();
                        move |_window, cx| {
                            let slider = cx.new(|_| {
                                SliderState::new()
                                    .min(ANCHOR_BPS_SLIDER_MIN)
                                    .max(ANCHOR_BPS_SLIDER_MAX)
                                    .step(ANCHOR_BPS_SLIDER_STEP)
                                    .default_value(
                                        anchor_bps_to_slider_value(min_bps)
                                            ..anchor_bps_to_slider_value(max_bps),
                                    )
                            });
                            let subscription = cx.subscribe(&slider, {
                                let editor = editor.clone();
                                move |_state: &mut SyncedAnchorRangeSliderState,
                                      _slider,
                                      event: &SliderEvent,
                                      cx| {
                                    let SliderEvent::Change(value) = event;
                                    editor.update(cx, |editor, cx| {
                                        set_broadcaster_anchor_bps_range(
                                            &mut editor.draft,
                                            value.start(),
                                            value.end(),
                                        );
                                        editor.draft_changed(cx);
                                    });
                                }
                            });
                            SyncedAnchorRangeSliderState {
                                slider,
                                synced_revision: revision,
                                _subscription: subscription,
                            }
                        }
                    },
                );

                state.update(cx, |state, cx| {
                    if state.synced_revision == revision {
                        return;
                    }
                    state.synced_revision = revision;
                    state.slider.update(cx, |slider, cx| {
                        slider.set_value(
                            anchor_bps_to_slider_value(min_bps)
                                ..anchor_bps_to_slider_value(max_bps),
                            window,
                            cx,
                        );
                    });
                });

                let slider = state.read(cx).slider.clone();
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div().flex().items_center().justify_end().child(
                            div()
                                .text_size(px(12.0))
                                .font_family(APP_MONO_FONT_FAMILY)
                                .text_color(rgb(theme::TEXT))
                                .child(format_anchor_bps_percent_range(min_bps, max_bps)),
                        ),
                    )
                    .child(Slider::new(&slider).w_full())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .text_size(px(12.0))
                            .line_height(px(16.0))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(format_anchor_premium_range(min_bps, max_bps))
                            .child(format!(
                                "Fees outside this range are marked suspicious. {}.",
                                format_anchor_bps_exact_range(min_bps, max_bps)
                            )),
                    )
            }),
        )
        .description("Transaction fees outside this percentage range are marked suspicious.")
        .layout(Axis::Vertical)
    }

    pub(in crate::root) fn number_field(
        field_id: impl Into<String>,
        editor: Entity<Self>,
        options: NumberFieldOptions,
        get: impl Fn(&WalletSettings) -> f64 + 'static,
        set: impl Fn(&mut WalletSettings, f64) + 'static,
    ) -> SettingField<SharedString> {
        let field_id = field_id.into();
        let get = Rc::new(get);
        let set = Rc::new(set);
        SettingField::render(move |render_options, window, cx| {
            let (value, revision) = {
                let editor = editor.read(cx);
                (get(&editor.draft), editor.field_sync_revision)
            };
            let value_text = SharedString::from(value.to_string());
            let state = window.use_keyed_state(
                SharedString::from(format!("wallet-settings-number-{field_id}")),
                cx,
                {
                    let value_text = value_text.clone();
                    let set = set.clone();
                    let editor = editor.clone();
                    let number_options = options.clone();
                    move |window, cx| {
                        let input = cx.new(|cx| {
                            InputState::new(window, cx).default_value(value_text.clone())
                        });
                        let subscriptions = vec![
                            cx.subscribe_in(&input, window, {
                                let number_options = number_options.clone();
                                move |_, input, event: &NumberInputEvent, window, cx| {
                                    let NumberInputEvent::Step(action) = event;
                                    input.update(cx, |input, cx| {
                                        if let Ok(value) = input.value().parse::<f64>() {
                                            let new_value = match action {
                                                StepAction::Increment => {
                                                    value + number_options.step
                                                }
                                                StepAction::Decrement => {
                                                    value - number_options.step
                                                }
                                            }
                                            .clamp(number_options.min, number_options.max);
                                            input.set_value(new_value.to_string(), window, cx);
                                        }
                                    });
                                }
                            }),
                            cx.subscribe_in(&input, window, {
                                let set = set.clone();
                                let editor = editor.clone();
                                let number_options = number_options.clone();
                                move |state: &mut SyncedNumberFieldState,
                                      input,
                                      event: &InputEvent,
                                      window,
                                      cx| {
                                    if !matches!(event, InputEvent::Change) {
                                        return;
                                    }
                                    if state.ignore_next_change {
                                        state.ignore_next_change = false;
                                        return;
                                    }
                                    input.update(cx, |input, cx| {
                                        let Ok(value) = input.value().parse::<f64>() else {
                                            return;
                                        };
                                        let clamped =
                                            value.clamp(number_options.min, number_options.max);
                                        let was_clamped = value < number_options.min
                                            || value > number_options.max;
                                        editor.update(cx, |editor, cx| {
                                            set(&mut editor.draft, clamped);
                                            editor.draft_changed(cx);
                                        });
                                        if was_clamped {
                                            state.ignore_next_change = true;
                                            input.set_value(clamped.to_string(), window, cx);
                                        }
                                    });
                                }
                            }),
                        ];
                        SyncedNumberFieldState {
                            input,
                            synced_revision: revision,
                            ignore_next_change: false,
                            _subscriptions: subscriptions,
                        }
                    }
                },
            );

            state.update(cx, |state, cx| {
                if state.synced_revision == revision {
                    return;
                }
                state.synced_revision = revision;
                if state.input.read(cx).value().as_ref() == value_text.as_ref() {
                    return;
                }
                state.ignore_next_change = true;
                state.input.update(cx, |input, cx| {
                    input.set_value(value_text.clone(), window, cx);
                });
            });

            let input = state.read(cx).input.clone();
            NumberInput::new(&input)
                .with_size(render_options.size)
                .map(|this| {
                    if matches!(render_options.layout, Axis::Horizontal) {
                        this.w_32()
                    } else {
                        this.w_full()
                    }
                })
        })
    }

    pub(in crate::root) fn dropdown_field(
        editor: Entity<Self>,
        options: Vec<(SharedString, SharedString)>,
        get: impl Fn(&WalletSettings) -> SharedString + 'static,
        set: impl Fn(&mut WalletSettings, SharedString) + 'static,
    ) -> SettingField<SharedString> {
        let get_editor = editor.clone();
        let set_editor = editor;
        SettingField::dropdown(
            options,
            move |cx| get(&get_editor.read(cx).draft),
            move |value, cx| {
                set_editor.update(cx, |editor, cx| {
                    set(&mut editor.draft, value);
                    editor.programmatic_draft_changed(cx);
                });
            },
        )
    }

    pub(in crate::root) fn open_settings_url_dialog(
        &self,
        kind: &SettingsUrlListKind,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let initial_value = index
            .and_then(|index| kind.endpoints(&self.draft).get(index).cloned())
            .unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial_value));
        let title = kind.dialog_title(index.is_some());
        let help = kind.dialog_help();
        let action_label = SharedString::from(if index.is_some() { "Save" } else { "Add" });
        let dialog_width = (window.viewport_size().width * 0.92).min(px(560.0));
        let dialog_max_height = dialog_max_height(window);
        let content_max_height = dialog_content_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let editor = cx.entity();
        let dialog_input = input.clone();
        let save_kind = kind.clone();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let save_editor = editor.clone();
            let save_input = dialog_input.clone();
            let save_kind = save_kind.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(title.clone()))
                .button_props(DialogButtonProps::default().ok_text(action_label.clone()))
                .footer(|ok, cancel, window, cx| vec![cancel(window, cx), ok(window, cx)])
                .on_ok(move |_event, _window, cx| {
                    let value = save_input.read(cx).value().trim().to_string();
                    let save_kind = save_kind.clone();
                    save_editor.update(cx, |editor, cx| {
                        match index {
                            Some(index) => save_kind.set_endpoint(&mut editor.draft, index, &value),
                            None => save_kind.add_endpoint(&mut editor.draft, &value),
                        }
                        editor.programmatic_draft_changed(cx);
                    });
                    true
                })
                .child(scrollable_dialog_content(
                    content_max_height,
                    render_settings_url_dialog_content(&dialog_input, content_width, help),
                ))
        });
        let focus_input = input;
        cx.defer_in(window, move |_editor, window, cx| {
            focus_input.read(cx).focus_handle(cx).focus(window);
        });
    }

    pub(in crate::root) fn render_settings_url_list(
        editor: &Entity<Self>,
        kind: &SettingsUrlListKind,
        endpoints: Vec<String>,
    ) -> gpui::Div {
        let add_editor = editor.clone();
        let add_kind = kind.clone();
        let body =
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(div().flex().justify_end().child(
                    settings_icon_button(kind.add_id(), IconName::Plus, "Add").on_click(
                        move |_event, window, cx| {
                            let kind = add_kind.clone();
                            add_editor.update(cx, |editor, cx| {
                                editor.open_settings_url_dialog(&kind, None, window, cx);
                            });
                        },
                    ),
                ));

        let endpoint_count = endpoints.len();
        let mut list = div().w_full().flex().flex_col();
        if endpoints.is_empty() {
            list = list.child(app_muted_text(kind.empty_text()).py(px(8.0)));
        }
        for (index, endpoint) in endpoints.into_iter().enumerate() {
            let edit_editor = editor.clone();
            let edit_kind = kind.clone();
            let remove_editor = editor.clone();
            let remove_kind = kind.clone();
            list = list.child(
                div()
                    .id(kind.row_id(index))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px(px(2.0))
                    .py(px(9.0))
                    .hover(|this| this.bg(rgb(theme::SURFACE_HOVER_SUBTLE)))
                    .when(index + 1 < endpoint_count, |this| {
                        this.border_b_1()
                            .border_color(rgb_with_alpha(theme::BORDER_SUBTLE, 0.45))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .font_family(APP_MONO_FONT_FAMILY)
                            .text_size(px(13.0))
                            .line_height(px(18.0))
                            .text_color(rgb(theme::TEXT))
                            .child(SharedString::from(endpoint)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .child(
                                settings_icon_button(
                                    kind.edit_id(index),
                                    Icon::new(RailgunActionIcon::Pencil),
                                    "Edit",
                                )
                                .on_click(
                                    move |_event, window, cx| {
                                        let kind = edit_kind.clone();
                                        edit_editor.update(cx, |editor, cx| {
                                            editor.open_settings_url_dialog(
                                                &kind,
                                                Some(index),
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ),
                            )
                            .child(
                                settings_danger_icon_button(
                                    kind.remove_id(index),
                                    Icon::new(RailgunActionIcon::Trash2),
                                    "Remove",
                                )
                                .on_click(
                                    move |_event, _window, cx| {
                                        let kind = remove_kind.clone();
                                        remove_editor.update(cx, |editor, cx| {
                                            kind.remove_endpoint(&mut editor.draft, index);
                                            editor.programmatic_draft_changed(cx);
                                        });
                                    },
                                ),
                            ),
                    ),
            );
        }
        body.child(list)
    }

    pub(in crate::root) fn settings_url_list_item(
        title: impl Into<SharedString>,
        editor: Entity<Self>,
        kind: SettingsUrlListKind,
        endpoints: Vec<String>,
    ) -> SettingItem {
        SettingItem::new(
            title,
            SettingField::<SharedString>::render(move |_options, _window, _cx| {
                Self::render_settings_url_list(&editor, &kind, endpoints.clone())
            }),
        )
        .layout(Axis::Vertical)
    }

    pub(in crate::root) fn open_waku_direct_peer_dialog(
        &self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let initial = index
            .and_then(|index| display_waku_direct_peers(&self.draft).get(index).cloned())
            .unwrap_or_default();
        let inputs = WakuDirectPeerDialogInputs {
            peer_id: cx.new(|cx| InputState::new(window, cx).default_value(initial.peer_id)),
            addr: cx.new(|cx| InputState::new(window, cx).default_value(initial.addr)),
        };
        let title = if index.is_some() {
            "Edit direct peer"
        } else {
            "Add direct peer"
        };
        let action_label = SharedString::from(if index.is_some() { "Save" } else { "Add" });
        let dialog_width = (window.viewport_size().width * 0.92).min(px(620.0));
        let dialog_max_height = dialog_max_height(window);
        let content_max_height = dialog_content_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let editor = cx.entity();
        let dialog_inputs = inputs.clone();
        let save_inputs = inputs.clone();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let save_editor = editor.clone();
            let save_inputs = save_inputs.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(title))
                .button_props(DialogButtonProps::default().ok_text(action_label.clone()))
                .footer(|ok, cancel, window, cx| vec![cancel(window, cx), ok(window, cx)])
                .on_ok(move |_event, _window, cx| {
                    let peer = waku_direct_peer_from_dialog_inputs(&save_inputs, cx);
                    save_editor.update(cx, |editor, cx| {
                        match index {
                            Some(index) => set_waku_direct_peer(&mut editor.draft, index, peer),
                            None => add_waku_direct_peer(&mut editor.draft, peer),
                        }
                        editor.programmatic_draft_changed(cx);
                    });
                    true
                })
                .child(scrollable_dialog_content(
                    content_max_height,
                    render_waku_direct_peer_dialog_content(&dialog_inputs, content_width),
                ))
        });
        let focus_input = inputs.peer_id;
        cx.defer_in(window, move |_editor, window, cx| {
            focus_input.read(cx).focus_handle(cx).focus(window);
        });
    }

    pub(in crate::root) fn render_waku_direct_peer_list(
        editor: &Entity<Self>,
        peers: Vec<WakuDirectPeerSetting>,
    ) -> gpui::Div {
        let add_editor = editor.clone();
        let body = div().w_full().flex().flex_col().gap_2().child(
            div().flex().justify_end().child(
                settings_icon_button(
                    "wallet-settings-waku-direct-peer-add",
                    IconName::Plus,
                    "Add",
                )
                .on_click(move |_event, window, cx| {
                    add_editor.update(cx, |editor, cx| {
                        editor.open_waku_direct_peer_dialog(None, window, cx);
                    });
                }),
            ),
        );

        let peer_count = peers.len();
        let mut list = div().w_full().flex().flex_col();
        if peers.is_empty() {
            list = list.child(app_muted_text("No additional direct peers configured.").py(px(8.0)));
        }
        for (index, peer) in peers.into_iter().enumerate() {
            let edit_editor = editor.clone();
            let remove_editor = editor.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!(
                        "wallet-settings-waku-direct-peer-row-{index}"
                    )))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px(px(2.0))
                    .py(px(9.0))
                    .hover(|this| this.bg(rgb(theme::SURFACE_HOVER_SUBTLE)))
                    .when(index + 1 < peer_count, |this| {
                        this.border_b_1()
                            .border_color(rgb_with_alpha(theme::BORDER_SUBTLE, 0.45))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .truncate()
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(px(13.0))
                                    .line_height(px(18.0))
                                    .text_color(rgb(theme::TEXT))
                                    .child(SharedString::from(peer.peer_id)),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(px(12.0))
                                    .line_height(px(16.0))
                                    .text_color(rgb(theme::TEXT_SUBTLE))
                                    .child(SharedString::from(peer.addr)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .child(
                                settings_icon_button(
                                    SharedString::from(format!(
                                        "wallet-settings-waku-direct-peer-edit-{index}"
                                    )),
                                    Icon::new(RailgunActionIcon::Pencil),
                                    "Edit",
                                )
                                .on_click(
                                    move |_event, window, cx| {
                                        edit_editor.update(cx, |editor, cx| {
                                            editor.open_waku_direct_peer_dialog(
                                                Some(index),
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ),
                            )
                            .child(
                                settings_danger_icon_button(
                                    SharedString::from(format!(
                                        "wallet-settings-waku-direct-peer-remove-{index}"
                                    )),
                                    Icon::new(RailgunActionIcon::Trash2),
                                    "Remove",
                                )
                                .on_click(
                                    move |_event, _window, cx| {
                                        remove_editor.update(cx, |editor, cx| {
                                            remove_waku_direct_peer(&mut editor.draft, index);
                                            editor.programmatic_draft_changed(cx);
                                        });
                                    },
                                ),
                            ),
                    ),
            );
        }
        body.child(list)
    }

    pub(in crate::root) fn waku_direct_peer_list_item(
        editor: Entity<Self>,
        peers: Vec<WakuDirectPeerSetting>,
    ) -> SettingItem {
        SettingItem::new(
            "Direct peers",
            SettingField::<SharedString>::render(move |_options, _window, _cx| {
                Self::render_waku_direct_peer_list(&editor, peers.clone())
            }),
        )
        .description(
            "Additional libp2p peers to dial directly. Each row is one peer ID and one multiaddr.",
        )
        .layout(Axis::Vertical)
    }

    pub(in crate::root) fn open_token_dialog(
        &self,
        target: &TokenEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let values = token_dialog_values(&self.draft, target);
        let inputs = TokenDialogInputs {
            chain_id: cx
                .new(|cx| InputState::new(window, cx).default_value(values.chain_id.to_string())),
            token_address: cx
                .new(|cx| InputState::new(window, cx).default_value(values.token_address.clone())),
            symbol: cx.new(|cx| InputState::new(window, cx).default_value(values.symbol.clone())),
            decimals: cx
                .new(|cx| InputState::new(window, cx).default_value(values.decimals.to_string())),
            icon_path: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(values.icon_path.clone().unwrap_or_default())
            }),
        };
        let title = match &target {
            TokenEditTarget::AddCustom => "Add custom token".to_string(),
            TokenEditTarget::BuiltIn(_) => "Edit built-in token".to_string(),
            TokenEditTarget::Custom(_) => "Edit custom token".to_string(),
        };
        let action_label = SharedString::from(if matches!(target, TokenEditTarget::AddCustom) {
            "Add"
        } else {
            "Save"
        });
        let readonly_identity = matches!(target, TokenEditTarget::BuiltIn(_));
        let dialog_width = (window.viewport_size().width * 0.92).min(px(620.0));
        let dialog_max_height = dialog_max_height(window);
        let content_max_height = dialog_content_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let editor = cx.entity();
        let save_inputs = inputs.clone();
        let save_target = target.clone();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let save_editor = editor.clone();
            let save_inputs = save_inputs.clone();
            let render_inputs = save_inputs.clone();
            let save_target = save_target.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(title.clone()))
                .button_props(DialogButtonProps::default().ok_text(action_label.clone()))
                .footer(|ok, cancel, window, cx| vec![cancel(window, cx), ok(window, cx)])
                .on_ok(move |_event, _window, cx| {
                    let values = match token_dialog_values_from_inputs(&save_inputs, cx) {
                        Ok(values) => values,
                        Err(error) => {
                            save_editor.update(cx, |editor, cx| {
                                editor.status = Some(Arc::from(error));
                                cx.notify();
                            });
                            return false;
                        }
                    };
                    let target = save_target.clone();
                    save_editor.update(cx, |editor, cx| {
                        apply_token_dialog_values(&mut editor.draft, &target, values);
                        editor.programmatic_draft_changed(cx);
                    });
                    true
                })
                .child(scrollable_dialog_content(
                    content_max_height,
                    render_token_dialog_content(&render_inputs, content_width, readonly_identity),
                ))
        });
        let focus_input = if readonly_identity {
            inputs.symbol
        } else {
            inputs.chain_id
        };
        cx.defer_in(window, move |_editor, window, cx| {
            focus_input.read(cx).focus_handle(cx).focus(window);
        });
    }

    pub(in crate::root) fn open_price_anchor_dialog(
        &self,
        target: &PriceAnchorEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let values = price_anchor_dialog_values(&self.draft, target);
        let chain_items = price_anchor_chain_select_items();
        let selected_chain_index = chain_select_index(&chain_items, values.chain_id);
        let selected_oracle_chain_index = chain_select_index(&chain_items, values.oracle_chain_id);
        let anchor_type_items = price_anchor_type_select_items();
        let selected_anchor_type_index =
            price_anchor_type_select_index(&anchor_type_items, values.anchor_type);
        let inputs = PriceAnchorDialogInputs {
            chain_id: cx
                .new(|cx| SelectState::new(chain_items.clone(), selected_chain_index, window, cx)),
            token_address: cx
                .new(|cx| InputState::new(window, cx).default_value(values.token_address.clone())),
            anchor_type: cx.new(|cx| {
                SelectState::new(anchor_type_items, selected_anchor_type_index, window, cx)
            }),
            selected_anchor_type: cx
                .new(|cx| InputState::new(window, cx).default_value(values.anchor_type)),
            fixed_rate: cx
                .new(|cx| InputState::new(window, cx).default_value(values.fixed_rate.clone())),
            oracle_chain_id: cx.new(|cx| {
                SelectState::new(chain_items.clone(), selected_oracle_chain_index, window, cx)
            }),
            oracle_address: cx
                .new(|cx| InputState::new(window, cx).default_value(values.oracle_address.clone())),
            oracle_token_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.oracle_token_decimals.clone())
            }),
            oracle_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.oracle_decimals.clone())
            }),
            oracle_is_inversed: cx.new(|cx| {
                SelectState::new(
                    bool_select_items(),
                    Some(bool_select_index(values.oracle_is_inversed)),
                    window,
                    cx,
                )
            }),
            twap_pool_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_pool_address.clone())
            }),
            twap_base_token_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_base_token_address.clone())
            }),
            twap_quote_token_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_quote_token_address.clone())
            }),
            twap_base_token_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_base_token_decimals.clone())
            }),
            twap_window_seconds: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_window_seconds.clone())
            }),
            product_scale_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.product_scale_decimals.clone())
            }),
            product_components: values
                .product_components
                .iter()
                .take(2)
                .map(|component| {
                    Self::product_anchor_component_dialog_inputs(
                        component,
                        &chain_items,
                        window,
                        cx,
                    )
                })
                .collect(),
        };
        let selected_anchor_type = inputs.selected_anchor_type.clone();
        Self::subscribe_price_anchor_type_select(
            &inputs.anchor_type,
            selected_anchor_type,
            window,
            cx,
        );
        for component in &inputs.product_components {
            Self::subscribe_price_anchor_type_select(
                &component.anchor_type,
                component.selected_anchor_type.clone(),
                window,
                cx,
            );
        }
        let viewport_size = window.viewport_size();
        let dialog_width = (viewport_size.width * 0.92).min(px(620.0));
        let dialog_max_height = dialog_max_height(window);
        let content_max_height = dialog_content_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let editor = cx.entity();
        let save_inputs = inputs.clone();
        let save_target = target.clone();
        let title = match target {
            PriceAnchorEditTarget::Add => "Add price anchor",
            PriceAnchorEditTarget::Edit(_) => "Edit price anchor",
        };
        let action_label = SharedString::from(if matches!(target, PriceAnchorEditTarget::Add) {
            "Add"
        } else {
            "Save"
        });
        window.open_dialog(cx, move |dialog, _window, cx| {
            let save_editor = editor.clone();
            let save_inputs = save_inputs.clone();
            let render_inputs = save_inputs.clone();
            let save_target = save_target.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(title))
                .button_props(DialogButtonProps::default().ok_text(action_label.clone()))
                .footer(|ok, cancel, window, cx| vec![cancel(window, cx), ok(window, cx)])
                .on_ok(move |_event, _window, cx| {
                    let anchor = match price_anchor_override_from_dialog_inputs(&save_inputs, cx) {
                        Ok(anchor) => anchor,
                        Err(error) => {
                            save_editor.update(cx, |editor, cx| {
                                editor.status = Some(Arc::from(error));
                                cx.notify();
                            });
                            return false;
                        }
                    };
                    let target = save_target.clone();
                    save_editor.update(cx, |editor, cx| {
                        apply_price_anchor_dialog_values(&mut editor.draft, &target, anchor);
                        editor.programmatic_draft_changed(cx);
                    });
                    true
                })
                .child(scrollable_dialog_content(
                    content_max_height,
                    render_price_anchor_dialog_content(&render_inputs, content_width, cx),
                ))
        });
        let focus_input = inputs.chain_id;
        cx.defer_in(window, move |_editor, window, cx| {
            focus_input.read(cx).focus_handle(cx).focus(window);
        });
    }

    pub(in crate::root) fn product_anchor_component_dialog_inputs(
        values: &PriceAnchorComponentDialogValues,
        chain_items: &[ChainSelectItem],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> ProductAnchorComponentDialogInputs {
        let component_type_items = product_component_type_select_items();
        let selected_component_type_index =
            price_anchor_type_select_index(&component_type_items, values.anchor_type);
        let selected_oracle_chain_index = chain_select_index(chain_items, values.oracle_chain_id);
        ProductAnchorComponentDialogInputs {
            anchor_type: cx.new(|cx| {
                SelectState::new(
                    component_type_items,
                    selected_component_type_index,
                    window,
                    cx,
                )
            }),
            selected_anchor_type: cx
                .new(|cx| InputState::new(window, cx).default_value(values.anchor_type)),
            fixed_rate: cx
                .new(|cx| InputState::new(window, cx).default_value(values.fixed_rate.clone())),
            oracle_chain_id: cx.new(|cx| {
                SelectState::new(
                    chain_items.to_vec(),
                    selected_oracle_chain_index,
                    window,
                    cx,
                )
            }),
            oracle_address: cx
                .new(|cx| InputState::new(window, cx).default_value(values.oracle_address.clone())),
            oracle_token_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.oracle_token_decimals.clone())
            }),
            oracle_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.oracle_decimals.clone())
            }),
            oracle_is_inversed: cx.new(|cx| {
                SelectState::new(
                    bool_select_items(),
                    Some(bool_select_index(values.oracle_is_inversed)),
                    window,
                    cx,
                )
            }),
            twap_pool_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_pool_address.clone())
            }),
            twap_base_token_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_base_token_address.clone())
            }),
            twap_quote_token_address: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_quote_token_address.clone())
            }),
            twap_base_token_decimals: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_base_token_decimals.clone())
            }),
            twap_window_seconds: cx.new(|cx| {
                InputState::new(window, cx).default_value(values.twap_window_seconds.clone())
            }),
        }
    }

    pub(in crate::root) fn subscribe_price_anchor_type_select(
        select: &Entity<SelectState<Vec<PriceAnchorTypeSelectItem>>>,
        selected_anchor_type: Entity<InputState>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        cx.subscribe_in(
            select,
            window,
            move |_editor,
                  _select,
                  event: &SelectEvent<Vec<PriceAnchorTypeSelectItem>>,
                  window,
                  cx| {
                if let SelectEvent::Confirm(Some(anchor_type)) = event {
                    selected_anchor_type.update(cx, |input, cx| {
                        input.set_value((*anchor_type).to_string(), window, cx);
                    });
                    cx.notify();
                }
            },
        )
        .detach();
    }

    pub(in crate::root) fn render_token_list(
        editor: &Entity<Self>,
        entries: Vec<DisplayTokenEntry>,
    ) -> gpui::Div {
        let add_editor = editor.clone();
        let body = div().w_full().flex().flex_col().gap_2().child(
            div().flex().child(
                app_button_base("wallet-settings-token-add")
                    .icon(IconName::Plus)
                    .outline()
                    .child(app_text("Add token"))
                    .on_click(move |_event, window, cx| {
                        add_editor.update(cx, |editor, cx| {
                            editor.open_token_dialog(&TokenEditTarget::AddCustom, window, cx);
                        });
                    }),
            ),
        );
        if entries.is_empty() {
            return body.child(app_muted_text("No tokens configured.").py(px(8.0)));
        }

        let mut list = div().w_full().flex().flex_col();
        let mut current_chain = None;
        let token_count = entries.len();
        for (index, entry) in entries.into_iter().enumerate() {
            if current_chain != Some(entry.chain_id) {
                current_chain = Some(entry.chain_id);
                list = list.child(settings_token_chain_header(entry.chain_id));
            }
            let edit_editor = editor.clone();
            let remove_editor = editor.clone();
            let edit_target = if entry.built_in {
                TokenEditTarget::BuiltIn(TokenKey {
                    chain_id: entry.chain_id,
                    token_address: entry.token_address.clone(),
                })
            } else {
                TokenEditTarget::Custom(entry.custom_index.unwrap_or(index))
            };
            let remove_index = entry.custom_index;
            list = list.child(
                div()
                    .id(SharedString::from(format!(
                        "wallet-settings-token-row-{}-{}",
                        entry.chain_id, entry.token_address
                    )))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px(px(2.0))
                    .py(px(9.0))
                    .hover(|this| this.bg(rgb(theme::SURFACE_HOVER_SUBTLE)))
                    .when(index + 1 < token_count, |this| {
                        this.border_b_1()
                            .border_color(rgb_with_alpha(theme::BORDER_SUBTLE, 0.45))
                    })
                    .child(render_token_entry_summary(&entry))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .child(
                                settings_icon_button(
                                    SharedString::from(format!(
                                        "wallet-settings-token-edit-{index}"
                                    )),
                                    Icon::new(RailgunActionIcon::Pencil),
                                    "Edit",
                                )
                                .on_click(
                                    move |_event, window, cx| {
                                        let target = edit_target.clone();
                                        edit_editor.update(cx, |editor, cx| {
                                            editor.open_token_dialog(&target, window, cx);
                                        });
                                    },
                                ),
                            )
                            .when(!entry.built_in, |this| {
                                this.child(
                                    settings_danger_icon_button(
                                        SharedString::from(format!(
                                            "wallet-settings-token-remove-{index}"
                                        )),
                                        Icon::new(RailgunActionIcon::Trash2),
                                        "Remove",
                                    )
                                    .on_click(
                                        move |_event, _window, cx| {
                                            if let Some(index) = remove_index {
                                                remove_editor.update(cx, |editor, cx| {
                                                    remove_custom_token(&mut editor.draft, index);
                                                    editor.programmatic_draft_changed(cx);
                                                });
                                            }
                                        },
                                    ),
                                )
                            }),
                    ),
            );
        }
        body.child(list)
    }

    pub(in crate::root) fn render_price_anchor_list(
        editor: &Entity<Self>,
        entries: Vec<DisplayPriceAnchorEntry>,
    ) -> gpui::Div {
        let add_editor = editor.clone();
        let body = div().w_full().flex().flex_col().gap_2().child(
            div().flex().child(
                app_button_base("wallet-settings-price-anchor-add")
                    .icon(IconName::Plus)
                    .outline()
                    .child(app_text("Add price anchor"))
                    .on_click(move |_event, window, cx| {
                        add_editor.update(cx, |editor, cx| {
                            editor.open_price_anchor_dialog(
                                &PriceAnchorEditTarget::Add,
                                window,
                                cx,
                            );
                        });
                    }),
            ),
        );

        if entries.is_empty() {
            return body.child(app_muted_text("No price anchors configured.").py(px(8.0)));
        }

        let mut list = div().w_full().flex().flex_col();
        let mut current_chain = None;
        let anchor_count = entries.len();
        for (index, entry) in entries.into_iter().enumerate() {
            if current_chain != Some(entry.key.chain_id) {
                current_chain = Some(entry.key.chain_id);
                list = list.child(settings_token_chain_header(entry.key.chain_id));
            }
            let edit_editor = editor.clone();
            let remove_editor = editor.clone();
            let edit_target = PriceAnchorEditTarget::Edit(entry.clone());
            let remove_entry = entry.clone();
            let can_remove = entry.override_index.is_some();
            list = list.child(
                div()
                    .id(SharedString::from(format!(
                        "wallet-settings-price-anchor-row-{}-{}",
                        entry.key.chain_id, entry.key.token_address
                    )))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px(px(2.0))
                    .py(px(9.0))
                    .hover(|this| this.bg(rgb(theme::SURFACE_HOVER_SUBTLE)))
                    .when(index + 1 < anchor_count, |this| {
                        this.border_b_1()
                            .border_color(rgb_with_alpha(theme::BORDER_SUBTLE, 0.45))
                    })
                    .child(render_price_anchor_entry_summary(&entry))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .child(
                                settings_icon_button(
                                    SharedString::from(format!(
                                        "wallet-settings-price-anchor-edit-{index}"
                                    )),
                                    Icon::new(RailgunActionIcon::Pencil),
                                    "Edit",
                                )
                                .on_click(
                                    move |_event, window, cx| {
                                        let target = edit_target.clone();
                                        edit_editor.update(cx, |editor, cx| {
                                            editor.open_price_anchor_dialog(&target, window, cx);
                                        });
                                    },
                                ),
                            )
                            .when(can_remove, |this| {
                                this.child(
                                    settings_danger_icon_button(
                                        SharedString::from(format!(
                                            "wallet-settings-price-anchor-remove-{index}"
                                        )),
                                        Icon::new(RailgunActionIcon::Trash2),
                                        "Remove",
                                    )
                                    .on_click(
                                        move |_event, _window, cx| {
                                            let entry = remove_entry.clone();
                                            remove_editor.update(cx, |editor, cx| {
                                                remove_display_price_anchor_override(
                                                    &mut editor.draft,
                                                    &entry,
                                                );
                                                editor.programmatic_draft_changed(cx);
                                            });
                                        },
                                    ),
                                )
                            }),
                    ),
            );
        }

        body.child(list)
    }

    pub(in crate::root) fn chain_quick_sync_endpoint_field(
        editor: Entity<Self>,
        chain_id: u64,
    ) -> SettingField<SharedString> {
        Self::shared_string_field(
            format!("chain-{chain_id}-quick-sync-endpoint"),
            editor,
            move |settings| display_chain_quick_sync_endpoint(settings, chain_id),
            move |settings, value| {
                settings
                    .chains
                    .per_chain
                    .entry(chain_id)
                    .or_default()
                    .quick_sync
                    .endpoint = non_empty_setting(&value);
            },
        )
    }

    pub(in crate::root) fn chain_contract_field(
        field_id: impl Into<String>,
        editor: Entity<Self>,
        chain_id: u64,
        get: impl Fn(&ChainContractSettings) -> Option<&String> + 'static,
        set: impl Fn(&mut ChainSettingsOverride, Option<String>) + 'static,
    ) -> SettingField<SharedString> {
        Self::shared_string_field(
            field_id,
            editor,
            move |settings| {
                let contracts = display_chain_contract_settings(settings, chain_id);
                get(&contracts).cloned().unwrap_or_default()
            },
            move |settings, value| {
                let chain = settings.chains.per_chain.entry(chain_id).or_default();
                set(chain, non_empty_setting(&value));
            },
        )
    }

    pub(in crate::root) fn chain_deployment_block_field(
        field_id: impl Into<String>,
        editor: Entity<Self>,
        chain_id: u64,
        get: impl Fn(&ChainDeploymentSettings) -> Option<u64> + 'static,
        set: impl Fn(&mut ChainDeploymentSettings, Option<u64>) + 'static,
    ) -> SettingField<SharedString> {
        Self::shared_string_field(
            field_id,
            editor,
            move |settings| {
                settings
                    .chains
                    .per_chain
                    .get(&chain_id)
                    .and_then(|chain| get(&chain.deployment))
                    .map_or_else(String::new, |value| value.to_string())
            },
            move |settings, value| {
                let chain = settings.chains.per_chain.entry(chain_id).or_default();
                set(&mut chain.deployment, optional_u64_setting(&value));
            },
        )
    }

    pub(in crate::root) fn chain_archive_rpc_field(
        editor: Entity<Self>,
        chain_id: u64,
    ) -> SettingField<SharedString> {
        Self::shared_string_field(
            format!("chain-{chain_id}-archive-rpc"),
            editor,
            move |settings| {
                settings
                    .chains
                    .per_chain
                    .get(&chain_id)
                    .and_then(|chain| chain.deployment.archive_rpc_url.clone())
                    .unwrap_or_default()
            },
            move |settings, value| {
                settings
                    .chains
                    .per_chain
                    .entry(chain_id)
                    .or_default()
                    .deployment
                    .archive_rpc_url = non_empty_setting(&value);
            },
        )
    }
}
