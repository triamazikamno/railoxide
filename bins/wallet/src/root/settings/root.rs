use super::{
    Arc, Context, Duration, Entity, HttpContext, ProverCacheBuildParams, ProverCacheBuildProgress,
    WalletNetworkConfig, WalletRoot, WalletSettings, WalletSettingsEditor,
    begin_prover_cache_build, build_cache_with_context_and_progress_with_session,
    build_effective_chain_configs, build_wallet_network_context, watch,
};
use crate::root::PublicActionMode;

impl WalletRoot {
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
            Ok(prepared) => {
                let mut params = prepared.params;
                if prepared.reuse_active_network {
                    params.reusable_http = Some(self.reusable_network_context());
                }
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
            self.effective_chain_configs = effective_chain_configs;
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
        if !self.public_form.shielding && self.public_form.action_progress.is_empty() {
            let shield_profile_changed =
                self.public_form.mimic_railway_shield != saved_mimic_railway_shield;
            self.public_form.mimic_railway_shield = saved_mimic_railway_shield;
            if shield_profile_changed {
                self.invalidate_public_action_gas_fee_quote(PublicActionMode::Shield);
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
        cx.notify();
    }
}
