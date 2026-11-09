use super::{
    Arc, Context, Duration, Entity, HttpContext, ProverCacheBuildParams, ProverCacheBuildProgress,
    WalletNetworkConfig, WalletRoot, WalletSettings, WalletSettingsEditor,
    begin_prover_cache_build, build_cache_with_context_and_progress_with_session,
    build_effective_chain_configs, build_wallet_network_context, data_folder_path,
    open_data_folder, watch,
};
use crate::root::PublicActionMode;

impl WalletRoot {
    pub(in crate::root) fn configured_native_symbol(&self, chain_id: u64) -> &str {
        self.effective_chain_configs
            .get(chain_id)
            .map_or("Unavailable", |chain| chain.native_currency.symbol.as_str())
    }

    pub(in crate::root) fn configured_native_amount_label(
        &self,
        chain_id: u64,
        amount: alloy::primitives::U256,
    ) -> String {
        self.effective_chain_configs.get(chain_id).map_or_else(
            || "Unavailable".to_owned(),
            |chain| chain.native_currency.format_amount(amount),
        )
    }

    /// The selected EVM chain, or `None` while Tron is selected.
    pub(in crate::root) const fn selected_evm_chain(&self) -> Option<u64> {
        self.selected_network.evm_chain_id()
    }

    pub(in crate::root) fn selected_chain_has_railgun(&self) -> bool {
        self.selected_evm_chain()
            .is_some_and(|chain_id| self.effective_chain_configs.railgun(chain_id).is_ok())
    }

    /// Why the Private and UTXOs tabs are unavailable on the selected network.
    pub(in crate::root) const fn railgun_unavailable_reason(&self) -> &'static str {
        match self.selected_network {
            crate::root::SelectedNetwork::Evm(_) => {
                "This chain supports Public accounts only; no Railgun deployment is configured."
            }
            crate::root::SelectedNetwork::Tron => {
                "Tron supports Public accounts only; Railgun is not available on Tron."
            }
        }
    }

    pub(in crate::root) const fn tron_enabled(&self) -> bool {
        self.tron_public.settings.enabled
    }

    /// Installs saved Tron settings and reconciles Tron's pricing with them. Disabling Tron
    /// while it is selected returns to the last EVM chain. EVM work kept running on that chain,
    /// so nothing restarts.
    pub(in crate::root) fn apply_saved_tron_enabled(
        &mut self,
        settings: &WalletSettings,
        cx: &mut Context<'_, Self>,
    ) {
        if !settings.tron.enabled && self.selected_network == crate::root::SelectedNetwork::Tron {
            self.selected_network = crate::root::SelectedNetwork::Evm(self.last_evm_chain);
            self.reset_public_asset_focus();
            cx.notify();
        }
        self.install_tron_settings(&settings.tron);
        self.sync_network_select(settings.tron.enabled, cx);
    }

    /// Rebuilds the header selector's choices and shows the selected network.
    fn sync_network_select(&self, tron_enabled: bool, cx: &mut Context<'_, Self>) {
        let items = crate::root::wallet_header::network_select_items(
            self.effective_chain_configs
                .enabled_chains()
                .map(|chain| (chain.chain_id, chain.name.clone())),
            tron_enabled,
        );
        let selected = self.selected_network;
        let select = self.chain_select.clone();
        let window = self.window_handle;
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                select.update(cx, |select, cx| {
                    select.set_items(ui::chain_select::ChainSelectItems::new(items), window, cx);
                    select.set_selected_value(&selected, window, cx);
                });
            });
        });
    }

    pub(in crate::root) fn admit_chain_settings(
        &self,
        settings: &WalletSettings,
    ) -> Result<(), String> {
        let next = build_effective_chain_configs(settings).map_err(|error| error.to_string())?;
        if !self.root_replacement_is_allowed()
            || self.public_transaction_cleanup.is_some()
            || self.public_sync_cache_resetting
            || self.merkle_forest_cache_resetting
        {
            return Err("Wait for wallet cleanup before changing chains".to_owned());
        }
        if self.public_form.sending
            && self.tron_send_owns_progress()
            && wallet_ops::tron::tron_rpc_route(&settings.tron) != self.tron_public.route
        {
            return Err(
                "Tron has an active send. Wait for it to finish before changing Tron settings."
                    .to_owned(),
            );
        }
        for (&id, previous) in self.effective_chain_configs.iter() {
            if next.get(id) == Some(previous) {
                continue;
            }
            if self.public_transaction_submissions.is_busy_on_chain(id)
                || self.public_transaction_tracker.has_pending_observation(id)
            {
                return Err(format!(
                    "Chain {id} has an active submission or transaction observation"
                ));
            }
        }
        Ok(())
    }

    pub(in crate::root) fn apply_saved_auto_lock_policy(&mut self, settings: &WalletSettings) {
        self.auto_lock.apply_policy(
            settings
                .runtime
                .auto_lock_timeout_secs
                .map(Duration::from_secs),
            self.vault_view_unlock.is_some(),
            crate::root::auto_lock::AutoLockTimestamp::now(),
        );
    }

    // GPUI requires a mutable context for Entity::update even though it is only forwarded here.
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) fn clear_settings_transient_status(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(editor) = self.settings_editor.clone() {
            editor.update(cx, |editor, cx| {
                editor.clear_transient_status(cx);
            });
        }
    }

    /// The editor outlives each Settings opening, so its folder size is measured again.
    // GPUI requires a mutable context for Entity::update even though it is only forwarded here.
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) fn invalidate_settings_data_folder_size(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(editor) = self.settings_editor.clone() {
            editor.update(cx, |editor, cx| {
                editor.invalidate_data_folder_size();
                cx.notify();
            });
        }
    }

    /// Opens the data folder from a Settings-unavailable row.
    pub(in crate::root) fn open_settings_data_folder(&mut self, cx: &mut Context<'_, Self>) {
        self.data_folder_open_error = None;
        cx.notify();
        let root = cx.weak_entity();
        open_data_folder(
            data_folder_path(&self.options.db_path),
            cx,
            move |error, cx| {
                let _ = root.update(cx, |root, cx| {
                    root.data_folder_open_error = Some(error);
                    cx.notify();
                });
            },
        );
    }

    pub(in crate::root) fn reusable_network_context(&self) -> HttpContext {
        self.http.clone()
    }

    pub(in crate::root) fn start_background_prover_cache_build(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        if self.is_prover_cache_building() {
            return;
        }
        let Some(editor) = self.settings_editor.clone() else {
            self.vault_error = Some(Arc::from(self.settings_error.as_ref().map_or_else(
                || "Settings are unavailable".to_string(),
                ToString::to_string,
            )));
            cx.notify();
            return;
        };
        let prepared = editor.update(cx, WalletSettingsEditor::prepare_prover_cache_build);
        let params = match prepared {
            Ok(mut params) => {
                params.reusable_http = Some(self.reusable_network_context());
                params
            }
            Err(message) => {
                editor.update(cx, |editor, cx| {
                    editor.status = Some(message);
                    cx.notify();
                });
                return;
            }
        };
        match self.start_prover_cache_build_from_settings(editor.clone(), params, cx) {
            Ok(()) => {
                editor.update(cx, |editor, cx| {
                    editor.mark_cache_build_started(ProverCacheBuildProgress::preparing(), cx);
                });
            }
            Err(message) => {
                editor.update(cx, |editor, cx| {
                    editor.status = Some(message);
                    cx.notify();
                });
            }
        }
    }

    pub(in crate::root) fn start_prover_cache_build_from_settings(
        &mut self,
        editor: Entity<WalletSettingsEditor>,
        params: ProverCacheBuildParams,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), Arc<str>> {
        if self.is_prover_cache_building() {
            return Err(Arc::from("Prover cache build is already running"));
        }

        let ProverCacheBuildParams {
            db,
            db_path,
            network_mode,
            proxy,
            reusable_http,
        } = params;
        let session = match begin_prover_cache_build(&db_path) {
            Ok(session) => session,
            Err(error) => return Err(Arc::from(error.to_string())),
        };
        let initial_progress = ProverCacheBuildProgress::preparing();
        self.prover_cache_build_completed = false;
        self.prover_cache_build_progress = Some(initial_progress.clone());
        self.prover_cache_build_popover_open = false;
        let (progress_tx, mut progress_rx) = watch::channel(initial_progress);
        let runtime = self.runtime.clone();
        let join = runtime.spawn(async move {
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
                        let succeeded = result.as_ref().is_ok_and(Result::is_ok);
                        let _ = this.update(cx, |root, cx| {
                            root.finish_prover_cache_build_progress(cx);
                            if succeeded {
                                root.prover_cache_build_completed = true;
                            }
                        });
                        editor.update(cx, |editor, cx| {
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
                        let _ = this.update(cx, |root, cx| {
                            root.update_prover_cache_build_progress(progress, cx);
                        });
                        editor.update(cx, |editor, cx| {
                            editor.cache_build_progress = Some(editor_progress);
                            cx.notify();
                        });
                    }
                }
            }
        })
        .detach();
        cx.notify();
        Ok(())
    }

    pub(in crate::root) fn apply_saved_request_settings(
        &mut self,
        settings: &WalletSettings,
        cx: &mut Context<'_, Self>,
    ) {
        let new_policy = settings.broadcaster.fee_policy();
        let fee_policy_bounds_changed = self.public_broadcaster_policy.min_anchor_bps
            != new_policy.min_anchor_bps
            || self.public_broadcaster_policy.max_anchor_bps != new_policy.max_anchor_bps;

        if let Ok(effective_chain_configs) = build_effective_chain_configs(settings) {
            let previous_selection = self.last_evm_chain;
            let changed: Vec<_> = self
                .effective_chain_configs
                .iter()
                .filter_map(|(&id, previous)| {
                    effective_chain_configs
                        .get(id)
                        .is_none_or(|next| !previous.operationally_matches(next))
                        .then_some(id)
                })
                .collect();
            for &id in &changed {
                self.http.rpc_broker().invalidate_block(id);
                self.gateway.drafts.borrow_mut().retire_chain(id);
            }
            let route_changes: Vec<_> = changed
                .iter()
                .copied()
                .filter(|id| {
                    match (
                        self.effective_chain_configs.get(*id),
                        effective_chain_configs.get(*id),
                    ) {
                        (Some(previous), Some(next)) => previous.rpc_route != next.rpc_route,
                        _ => true,
                    }
                })
                .collect();
            self.public_broadcaster_anchor_refresh
                .reconcile_native_sources(&effective_chain_configs, &route_changes);
            self.effective_chain_configs = effective_chain_configs;
            self.refresh_walletconnect_fee_usd_values();
            if self
                .effective_chain_configs
                .enabled(self.last_evm_chain)
                .is_err()
                && let Some(chain) = self.effective_chain_configs.enabled_chains().next()
            {
                self.last_evm_chain = chain.chain_id;
                if self.selected_network != crate::root::SelectedNetwork::Tron {
                    self.selected_network = crate::root::SelectedNetwork::Evm(chain.chain_id);
                }
                self.ui_state.last_chain_id = Some(chain.chain_id);
                self.save_ui_state();
            }
            self.install_chain_background();
            self.sync_network_select(settings.tron.enabled, cx);
            if changed.contains(&previous_selection) || previous_selection != self.last_evm_chain {
                self.clear_public_chain_balance_state();
                self.restore_public_balance_snapshot();
                self.invalidate_advanced_public_send_estimate();
                self.invalidate_public_action_gas_fee_quote(PublicActionMode::Send);
                self.invalidate_public_action_gas_fee_quote(PublicActionMode::Shield);
                self.invalidate_public_action_gas_fee_quote(PublicActionMode::Wrap);
                self.invalidate_public_action_gas_fee_quote(PublicActionMode::Unwrap);
                self.schedule_automatic_public_balance_refresh(cx);
            }
            if !self.selected_chain_has_railgun() {
                self.active_wallet_tab = crate::root::shell::WalletTab::Public;
                self.send_forms.clear();
                self.unshield_forms.clear();
                self.private_action_form = None;
            }
            self.publish_gateway_desktop_state();
        }
        self.public_broadcaster_policy = new_policy;
        self.public_broadcaster_response_timeout =
            Duration::from_secs(settings.broadcaster.response_timeout_secs);
        self.public_broadcaster_republish_interval =
            Duration::from_secs(settings.broadcaster.republish_interval_secs);
        self.default_allow_suspicious_broadcasters = settings
            .broadcaster
            .allow_suspicious_broadcasters_by_default;
        let saved_mimic_railway_shield = settings.privacy.mimic_railway_shields_by_default;
        self.mimic_railway_shields_by_default = saved_mimic_railway_shield;
        self.unwrap_unshields_by_default = settings.privacy.unwrap_unshields_by_default;
        if !self.public_form.shielding && self.public_form.action_progress.is_empty() {
            let shield_profile_changed =
                self.public_form.mimic_railway_shield != saved_mimic_railway_shield;
            self.public_form.mimic_railway_shield = saved_mimic_railway_shield;
            if shield_profile_changed {
                self.invalidate_public_action_gas_fee_quote(PublicActionMode::Shield);
                self.public_form.native_shield_preparation.invalidate();
            }
        }

        if fee_policy_bounds_changed {
            for form in self.send_forms.values_mut() {
                form.cost_estimate = None;
                form.estimate_id = 0;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
            }
            for form in self.unshield_forms.values_mut() {
                form.cost_estimate = None;
                form.estimate_id = 0;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
            }
        }

        self.ensure_walletconnect_relay_processing(cx);
        self.publish_gateway_desktop_state();
        cx.notify();
    }
}
