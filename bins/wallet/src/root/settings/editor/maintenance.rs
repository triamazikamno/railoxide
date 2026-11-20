use super::*;

impl WalletSettingsEditor {
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
        window.open_wallet_alert_dialog(cx, move |dialog, _window, _cx| {
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
        window.open_wallet_alert_dialog(cx, move |dialog, _window, _cx| {
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
        window.open_wallet_alert_dialog(cx, move |dialog, _window, _cx| {
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

    pub(in crate::root) const fn invalidate_data_folder_size(&mut self) {
        self.data_folder_size = DataFolderSize::Unknown;
    }

    fn start_data_folder_size(&mut self, cx: &mut Context<'_, Self>) {
        if self.data_folder_size != DataFolderSize::Unknown {
            return;
        }
        self.data_folder_size_generation += 1;
        let generation = self.data_folder_size_generation;
        self.data_folder_size = DataFolderSize::Calculating(generation);
        let path = self.data_folder.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { measure_data_folder(&path) })
                .await;
            let _ = this.update(cx, |editor, cx| {
                editor.finish_data_folder_size(generation, result, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// Applies a walk's result unless a later invalidation or walk superseded it.
    pub(in crate::root) fn finish_data_folder_size(
        &mut self,
        generation: u64,
        result: DataFolderSize,
        cx: &mut Context<'_, Self>,
    ) {
        if self.data_folder_size == DataFolderSize::Calculating(generation) {
            self.data_folder_size = result;
            cx.notify();
        }
    }

    pub(in crate::root) fn render_data_folder_action(
        editor: &Entity<Self>,
        window: &Window,
        cx: &mut App,
    ) -> gpui::Div {
        let (path, size, open_error) = {
            let state = editor.read(cx);
            (
                state.data_folder.clone(),
                state.data_folder_size,
                state.data_folder_open_error.clone(),
            )
        };
        if size == DataFolderSize::Unknown {
            // Render cannot update the editor, so the walk starts after this frame.
            let start_editor = editor.clone();
            window.defer(cx, move |_window, cx| {
                start_editor.update(cx, Self::start_data_folder_size);
            });
        }
        let size_label = match size {
            DataFolderSize::Unknown | DataFolderSize::Calculating(_) => {
                app_muted_shimmer_text("Calculating...")
            }
            DataFolderSize::Ready(bytes) => app_muted_text(ui::format::format_decimal_bytes(bytes)),
            DataFolderSize::Missing => app_muted_text("Not created"),
        };
        let open_editor = editor.clone();
        let open_path = path.clone();
        data_folder_row(
            &path,
            Some(size_label),
            size != DataFolderSize::Missing,
            open_error,
            move |_window, cx| {
                open_editor.update(cx, |editor, cx| {
                    editor.data_folder_open_error = None;
                    cx.notify();
                });
                let error_editor = open_editor.downgrade();
                open_data_folder(open_path.clone(), cx, move |error, cx| {
                    let _ = error_editor.update(cx, |editor, cx| {
                        editor.data_folder_open_error = Some(error);
                        cx.notify();
                    });
                });
            },
        )
    }

    pub(in crate::root) fn render_build_prover_cache_action(
        editor: &Entity<Self>,
        cx: &App,
    ) -> gpui::Div {
        let (building, invalid, pre_start) = {
            let state = editor.read(cx);
            (
                state.cache_building,
                state.validation_error.is_some(),
                state.pre_start,
            )
        };
        let label = if building {
            "Building..."
        } else {
            "Build prover cache"
        };
        let build_editor = editor.clone();
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                app_button("wallet-settings-build-cache", label)
                    // The build opens its own network context.
                    .disabled(building || invalid || pre_start)
                    .on_click(move |_event, window, cx| {
                        build_editor.update(cx, |editor, cx| {
                            editor.request_prover_cache_build(window, cx);
                        });
                    }),
            )
            .when(pre_start, |row| {
                row.child(app_muted_text("Available after setup."))
            })
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

    pub(in crate::root) fn prepare_prover_cache_build(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Result<ProverCacheBuildParams, Arc<str>> {
        if self.cache_building || self.cache_build_progress.is_some() {
            return Err(Arc::from("Prover cache build is already running"));
        }
        self.refresh_validation();
        if self.validation_error.is_some() {
            return Err(Arc::from(
                "Fix validation errors before building prover cache",
            ));
        }
        // Saved network settings match the active root; unsaved network changes apply only after restart.
        let proxy = self
            .saved
            .network
            .proxy_url
            .as_deref()
            .map(reqwest::Url::parse)
            .transpose()
            .map_err(|error| Arc::from(format!("Invalid proxy URL: {error}")))?;
        let db = self.vault_store.db();
        let db_path = db.root_dir().to_path_buf();
        let network_mode = self.saved.wallet_network_mode();
        cx.notify();
        Ok(ProverCacheBuildParams {
            db,
            db_path,
            network_mode,
            proxy,
            reusable_http: None,
        })
    }

    pub(in crate::root) fn request_prover_cache_build(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.draft.network == self.saved.network {
            self.build_prover_cache(cx);
            return;
        }
        let body = match self.saved.network.mode {
            NetworkModeSetting::Tor => {
                "Your network changes are not saved. The prover cache download will use your saved setting, Tor."
            }
            NetworkModeSetting::Proxy => {
                "Your network changes are not saved. The prover cache download will use your saved proxy."
            }
            NetworkModeSetting::Direct => {
                "Your network changes are not saved. The prover cache download will use your saved setting, Direct, which connects without Tor or a proxy."
            }
        };
        let editor = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(520.0));
        let dialog_max_height = dialog_max_height(window);
        window.open_wallet_alert_dialog(cx, move |dialog, _window, _cx| {
            let confirm_editor = editor.clone();
            confirmation_dialog(
                dialog,
                ConfirmationDialogProps::primary(
                    "Unsaved network changes",
                    body,
                    Some("Apply your network changes first to download with the new setting."),
                    "Build with saved setting",
                ),
                dialog_width,
                dialog_max_height,
            )
            .on_ok(move |_event, _window, cx| {
                confirm_editor.update(cx, |editor, cx| {
                    editor.build_prover_cache(cx);
                });
                true
            })
        });
    }

    pub(in crate::root) fn build_prover_cache(&mut self, cx: &mut Context<'_, Self>) {
        let params = match self.prepare_prover_cache_build(cx) {
            Ok(params) => params,
            Err(message) => {
                self.status = Some(message);
                cx.notify();
                return;
            }
        };
        let initial_progress = ProverCacheBuildProgress::preparing();
        if let Some(root) = self.active_root.as_ref() {
            let editor = cx.entity();
            let root_params = params.clone();
            let start = root.update(cx, |root, cx| {
                let mut params = root_params;
                params.reusable_http = Some(root.reusable_network_context());
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
                    tracing::debug!(target: LOG_TARGET, %error, "falling back to local prover cache build task");
                }
            }
        }

        self.start_local_prover_cache_build(params, initial_progress, cx);
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
                            editor.invalidate_data_folder_size();
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
}
