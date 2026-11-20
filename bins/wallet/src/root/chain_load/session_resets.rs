#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn reset_wallet_scoped_state(&mut self, cx: &mut Context<'_, Self>) {
        self.invalidate_governance_context();
        self.governance.proposal_participation.clear_session();
        self.invalidate_proposals_chain(self.last_evm_chain);
        self.clear_protected_software_seed_session(cx);
        self.send_forms.clear();
        self.unshield_forms.clear();
        self.set_broadcaster_preferences(wallet_ops::vault::BroadcasterPreferences::default(), cx);
        self.broadcaster_preferences.error = None;
        self.broadcaster_preferences.active_tab = BroadcasterActivityTab::default();
        self.clear_public_wallet_runtime_state(cx);
        self.private_action_form = None;
        self.clear_private_broadcaster_progress_state();
        self.broadcaster_picker = None;
        self.blocked_shield_rescue_rows.clear();
        self.blocked_shield_refunds_in_flight.clear();
        self.blocked_shield_rescue_lookup_generation =
            self.blocked_shield_rescue_lookup_generation.wrapping_add(1);
        self.pending_ppoi_validation_toast = None;
        // The next wallet opens on the same tab, unless its chain can't show it.
        if self.active_wallet_tab.needs_railgun() && !self.selected_chain_has_railgun() {
            self.active_wallet_tab = self.default_wallet_tab();
        }
        for state in self.chain_states.values_mut() {
            *state = ChainUtxoState::Idle;
        }
        self.poi_artifact_cache_retry_attempts.clear();
        self.sync_utxo_table(cx);
    }

    pub(in crate::root) fn shutdown_wallet_session_store(&mut self) {
        self.clear_private_broadcaster_progress_state();
        let cleanup = self.wallet_sync_lifecycle.invalidate();
        self.start_wallet_sync_cleanup(cleanup);
    }

    pub(in crate::root) fn begin_public_sync_cache_reset(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> WalletPublicSyncCacheResetContext {
        // The forms outlive the aborted submission.
        self.pin_abandoned_private_broadcaster_payload();
        self.begin_public_transaction_shutdown();
        self.clear_private_broadcaster_progress_state();
        self.public_sync_cache_resetting = true;
        self.advance_active_wallet_generation();
        let session_store = self.wallet_sync_lifecycle.public_sync_cache_reset_cell();
        let cleanup = self.wallet_sync_lifecycle.supersede_wallet();
        self.start_wallet_sync_cleanup(cleanup);
        for state in self.chain_states.values_mut() {
            *state = ChainUtxoState::Idle;
        }
        self.poi_artifact_cache_retry_attempts.clear();
        self.sync_utxo_table(cx);
        cx.notify();
        WalletPublicSyncCacheResetContext {
            cleanup: self.wallet_sync_cleanup_wait_group(),
            session_store,
        }
    }

    pub(in crate::root) fn begin_poi_data_reset(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> WalletPublicSyncCacheResetContext {
        // The forms outlive the aborted submission.
        self.pin_abandoned_private_broadcaster_payload();
        self.begin_public_transaction_shutdown();
        self.clear_private_broadcaster_progress_state();
        self.public_sync_cache_resetting = true;
        self.advance_active_wallet_generation();
        let session_store = self.wallet_sync_lifecycle.public_sync_cache_reset_cell();
        let cleanup = self.wallet_sync_lifecycle.invalidate();
        self.start_wallet_sync_cleanup(cleanup);
        for state in self.chain_states.values_mut() {
            *state = ChainUtxoState::Idle;
        }
        self.poi_artifact_cache_retry_attempts.clear();
        self.sync_utxo_table(cx);
        cx.notify();
        WalletPublicSyncCacheResetContext {
            cleanup: self.wallet_sync_cleanup_wait_group(),
            session_store,
        }
    }

    pub(in crate::root) fn finish_public_sync_cache_reset(
        &mut self,
        restart_safe: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.public_sync_cache_resetting = false;
        self.resume_public_transactions();
        if restart_safe && self.view_session.is_some() {
            self.ensure_chain_load(self.last_evm_chain, cx);
        } else {
            cx.notify();
        }
    }

    pub(in crate::root) fn supersede_wallet_sessions(&mut self) {
        let cleanup = self.wallet_sync_lifecycle.supersede_wallet();
        self.start_wallet_sync_cleanup(cleanup);
    }

    pub(in crate::root) fn begin_wallet_deletion_sync_shutdown(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> WalletSyncLifecycleCleanupWaitGroup {
        self.begin_public_transaction_shutdown();
        self.advance_active_wallet_generation();
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
        self.revealed_passphrase_context_id = None;
        let cleanup = self.wallet_sync_lifecycle.invalidate();
        self.start_wallet_sync_cleanup(cleanup);
        self.reset_wallet_scoped_state(cx);
        cx.notify();
        self.wallet_sync_cleanup_wait_group()
    }

    pub(in crate::root) fn begin_root_replacement_shutdown(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> (
        WalletRootReplacementCleanup,
        HttpContext,
        wallet_ops::PublicTransactionTracker,
    ) {
        self.begin_public_transaction_shutdown();
        self.advance_active_wallet_generation();
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
        self.revealed_passphrase_context_id = None;
        self.wallet_sync_lifecycle_shutdown_started = true;
        self.cancel_tor_rotation();
        let waku_completion = self.stop_waku_for_root_replacement();
        let proposal_cleanup = self.proposals.take_cleanup();
        let cleanup = self.wallet_sync_lifecycle.invalidate();
        // A retained Tor context must not keep warming the outgoing routes.
        self.rpc_identity_warmup = None;
        self.active_rpc_chains.clear();
        self.public_broadcaster_anchor_refresh
            .set_active_chains(&[]);
        let outgoing_http = self.http.clone();
        self.reset_wallet_scoped_state(cx);
        self.start_wallet_sync_cleanup(cleanup);
        let sync_cleanup = self.wallet_sync_cleanup_wait_group();
        let root_replacement_cleanup = WalletRootReplacementCleanup::spawn(
            &self.runtime,
            sync_cleanup,
            waku_completion,
            proposal_cleanup,
            self.gateway.take_shutdown_handle(),
            self.monitor_state.clone(),
            self.monitor_event_tx.clone(),
        );
        (
            root_replacement_cleanup,
            outgoing_http,
            self.public_transaction_tracker.successor(),
        )
    }

    fn start_wallet_sync_cleanup(
        &mut self,
        cleanup: WalletSyncLifecycleCleanup,
    ) -> WalletSyncLifecycleCleanupTask {
        self.prune_finished_wallet_sync_cleanups();
        let task = cleanup.spawn(&self.runtime);
        self.wallet_sync_cleanup_tasks.push(task.clone());
        task
    }

    pub(super) fn prune_finished_wallet_sync_cleanups(&mut self) {
        self.wallet_sync_cleanup_tasks
            .retain(|cleanup| !cleanup.is_finished());
    }

    pub(in crate::root) fn destructive_cache_reset_is_allowed(&mut self) -> bool {
        self.prune_finished_wallet_sync_cleanups();
        destructive_cache_reset_admission_is_allowed(
            self.manage_wallets.deleting_wallet_id.is_some(),
            !self.wallet_sync_cleanup_tasks.is_empty()
                || self
                    .public_transaction_cleanup
                    .as_ref()
                    .is_some_and(|cleanup| !cleanup.is_finished()),
        )
    }

    pub(in crate::root) fn wallet_sync_cleanup_wait_group(
        &mut self,
    ) -> WalletSyncLifecycleCleanupWaitGroup {
        self.prune_finished_wallet_sync_cleanups();
        let mut cleanup =
            WalletSyncLifecycleCleanupWaitGroup::new(self.wallet_sync_cleanup_tasks.clone());
        cleanup
            .public_transactions
            .clone_from(&self.public_transaction_cleanup);
        cleanup
    }

    pub(in crate::root) fn begin_merkle_forest_cache_reset(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> WalletSyncLifecycleCleanupWaitGroup {
        self.begin_public_transaction_shutdown();
        self.merkle_forest_cache_resetting = true;
        self.advance_active_wallet_generation();
        let cleanup = self.wallet_sync_lifecycle.invalidate();
        self.start_wallet_sync_cleanup(cleanup);
        self.send_forms.clear();
        self.unshield_forms.clear();
        self.private_action_form = None;
        self.clear_private_broadcaster_progress_state();
        self.broadcaster_picker = None;
        for state in self.chain_states.values_mut() {
            *state = ChainUtxoState::Idle;
        }
        self.poi_artifact_cache_retry_attempts.clear();
        self.sync_utxo_table(cx);
        cx.notify();
        self.wallet_sync_cleanup_wait_group()
    }

    pub(in crate::root) fn finish_merkle_forest_cache_reset(
        &mut self,
        reset_succeeded: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.merkle_forest_cache_resetting = false;
        self.resume_public_transactions();
        self.prune_finished_wallet_sync_cleanups();
        if reset_succeeded && self.view_session.is_some() {
            self.ensure_chain_load(self.last_evm_chain, cx);
        } else {
            cx.notify();
        }
    }
}
