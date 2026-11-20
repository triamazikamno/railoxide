#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn selected_wallet_source(&self) -> WalletSource {
        let Some(selected_wallet_id) = self.selected_wallet_id.as_ref() else {
            return WalletSource::Imported;
        };
        self.wallet_options
            .iter()
            .find(|option| option.wallet_id.as_ref() == selected_wallet_id.as_ref())
            .map_or(WalletSource::Imported, |option| option.source)
    }

    pub(super) fn selected_wallet_sync_start_policy(&self) -> DesktopWalletSyncStartPolicy {
        let Some(selected_wallet_id) = self.selected_wallet_id.as_ref() else {
            return DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill;
        };
        self.wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == selected_wallet_id.as_ref())
            .map_or(
                DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
                DesktopWalletSyncStartPolicy::from,
            )
    }

    pub(in crate::root) fn selected_chain_wallet_start_block(&self) -> Option<u64> {
        self.chain_states
            .get(&self.last_evm_chain)
            .and_then(ChainUtxoState::start_block)
    }

    pub(in crate::root) fn selected_chain_poi_artifact_progress(
        &self,
    ) -> Option<&PoiArtifactCacheProgress> {
        self.poi_artifact_cache_progress.get(&self.last_evm_chain)
    }

    pub(in crate::root) fn is_active_wallet_generation(
        &self,
        wallet_id: &str,
        generation: u64,
    ) -> bool {
        wallet_generation_matches(
            self.selected_wallet_id.as_deref(),
            self.active_wallet_generation,
            wallet_id,
            generation,
        )
    }

    pub(in crate::root) fn install_chain_background(&mut self) {
        if self.wallet_sync_lifecycle_shutdown_started {
            return;
        }
        self.rpc_identity_warmup = Some(wallet_ops::start_rpc_identity_warmup(
            &self.http,
            &self.effective_chain_configs,
            self.last_evm_chain,
        ));
        let chains = self
            .effective_chain_configs
            .enabled_chains()
            .filter(|chain| {
                chain.require_railgun().is_ok()
                    || chain.chain_id == self.last_evm_chain
                    || self.active_rpc_chains.contains(&chain.chain_id)
                    || self
                        .public_transaction_tracker
                        .has_pending_observation(chain.chain_id)
            })
            .map(|chain| chain.chain_id)
            .collect::<Vec<_>>();
        self.active_rpc_chains.clear();
        // The new warmup hasn't seen Tron's route yet.
        self.tron_public.route_warmed = false;
        self.activate_chains_background(chains);
    }

    pub(in crate::root) fn activate_chain_background(&mut self, chain_id: u64) {
        self.activate_chains_background([chain_id]);
    }

    pub(in crate::root) fn activate_chains_background(
        &mut self,
        chains: impl IntoIterator<Item = u64>,
    ) {
        if self.wallet_sync_lifecycle_shutdown_started {
            return;
        }
        for chain_id in chains {
            let Ok(chain) = self.effective_chain_configs.enabled(chain_id) else {
                continue;
            };
            if self.active_rpc_chains.insert(chain_id)
                && let Some(warmup) = &self.rpc_identity_warmup
            {
                warmup.activate_route(chain.rpc_route.clone());
            }
        }
        self.sync_active_pricing_chains();
    }

    /// Native USD sources refresh for the active EVM chains, and for Tron once it has been
    /// selected this session. A custom EVM chain with Tron's chain id never activates Tron's
    /// pricing.
    pub(in crate::root) fn sync_active_pricing_chains(&mut self) {
        let tron_id = wallet_ops::tron::TRON_CHAIN_ID;
        let mut chains: Vec<u64> = self
            .active_rpc_chains
            .iter()
            .copied()
            .filter(|&chain_id| chain_id != tron_id)
            .collect();
        if self.tron_public.activated
            && let Some(route) = &self.tron_public.route
        {
            chains.push(tron_id);
            if !self.tron_public.route_warmed
                && let Some(warmup) = &self.rpc_identity_warmup
            {
                warmup.activate_route(route.clone());
                self.tron_public.route_warmed = true;
            }
        }
        self.public_broadcaster_anchor_refresh
            .set_active_chains(&chains);
    }

    pub(in crate::root) fn select_chain(
        &mut self,
        chain_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.effective_chain_configs.enabled(chain_id).is_err() {
            return;
        }
        self.show_network_in_selector(SelectedNetwork::Evm(chain_id), window, cx);
        if self.selected_network == SelectedNetwork::Evm(chain_id) {
            return;
        }
        window.close_all_dialogs(cx);
        self.selected_network = SelectedNetwork::Evm(chain_id);
        self.last_evm_chain = chain_id;
        self.activate_chain_background(chain_id);
        if let Some(warmup) = &self.rpc_identity_warmup {
            warmup.prioritize_chain(chain_id);
        }
        if !self.selected_chain_has_railgun() {
            self.active_wallet_tab = WalletTab::Public;
        }
        self.invalidate_proposals_chain(chain_id);
        self.invalidate_governance_context();
        self.ui_state.last_chain_id = Some(chain_id);
        self.save_ui_state();
        self.sync_broadcaster_monitor_chain_filter(chain_id, window, cx);
        self.clear_private_action_state();
        self.clear_public_chain_balance_state();
        self.restore_public_balance_snapshot();
        self.sync_utxo_table(cx);
        if self.active_wallet_tab == WalletTab::Public || self.gateway_has_connected_browser() {
            self.schedule_automatic_public_balance_refresh(cx);
        }
        self.publish_gateway_desktop_state();
        if should_focus_utxo_table(
            self.active_activity,
            self.active_wallet_tab,
            self.chain_states.get(&chain_id),
        ) {
            self.focus_utxo_table_on_render = true;
        }
        if self.view_session.is_some() {
            self.ensure_chain_load(chain_id, cx);
        }
        if self.active_activity == crate::root::sidebar::Activity::Proposals {
            match self.governance.tab {
                crate::root::governance::GovernanceTab::Proposals => {
                    self.start_proposals_refresh(false, cx);
                }
                crate::root::governance::GovernanceTab::Staking => self.start_staking_refresh(cx),
            }
        }
        cx.notify();
    }

    /// Selects Tron without persisting it. Chain loads, private sync, balances and the gateway
    /// keep following `last_evm_chain`.
    pub(in crate::root) fn select_tron(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !self.tron_enabled() {
            return;
        }
        self.show_network_in_selector(SelectedNetwork::Tron, window, cx);
        if self.selected_network == SelectedNetwork::Tron {
            return;
        }
        window.close_all_dialogs(cx);
        self.selected_network = SelectedNetwork::Tron;
        self.activate_tron();
        self.reset_public_asset_focus();
        self.reconcile_public_account_selection();
        self.schedule_automatic_public_balance_refresh(cx);
        self.active_wallet_tab = WalletTab::Public;
        self.invalidate_proposals_chain(self.last_evm_chain);
        self.invalidate_governance_context();
        self.clear_private_action_state();
        self.refresh_wallet_activity(cx);
        if self.active_activity == crate::root::sidebar::Activity::Proposals
            && self.governance.tab == crate::root::governance::GovernanceTab::Proposals
        {
            self.start_proposals_refresh(false, cx);
        }
        cx.notify();
    }

    /// Drops the private forms and broadcaster state of the network being left.
    fn clear_private_action_state(&mut self) {
        self.send_forms.clear();
        self.unshield_forms.clear();
        self.private_action_form = None;
        self.clear_private_broadcaster_progress_state();
        self.broadcaster_picker = None;
        self.local_pending_spent_clear_confirming = false;
    }

    fn show_network_in_selector(
        &self,
        network: SelectedNetwork,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.chain_select.read(cx).selected_value() != Some(&network) {
            self.chain_select.update(cx, |select, cx| {
                select.set_selected_value(&network, window, cx);
                cx.notify();
            });
        }
    }

    pub(in crate::root) fn sync_broadcaster_monitor_chain_filter(
        &self,
        chain_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.monitor.update(cx, |monitor, cx| {
            monitor.set_chain_filter(chain_id, window, cx);
        });
    }
}
