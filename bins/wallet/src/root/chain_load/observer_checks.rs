#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(super) fn is_current_chain_load_startup(
        &self,
        wallet_id: &str,
        active_wallet_generation: u64,
        chain_id: u64,
        lifecycle_generation: u64,
        task_id: u64,
    ) -> bool {
        self.is_active_wallet_generation(wallet_id, active_wallet_generation)
            && self.wallet_sync_lifecycle.is_current_startup(
                chain_id,
                lifecycle_generation,
                task_id,
            )
    }

    pub(super) fn is_current_installed_observer(
        &self,
        wallet_id: &str,
        active_wallet_generation: u64,
        chain_id: u64,
        observer_token: &InstalledObserverToken,
    ) -> bool {
        installed_observer_is_exact_current(
            self.selected_wallet_id.as_deref(),
            self.active_wallet_generation,
            wallet_id,
            active_wallet_generation,
            chain_id,
            self.chain_states
                .get(&chain_id)
                .and_then(ChainUtxoState::installed_observer_token),
            observer_token,
        )
    }

    /// Drops the broker's cached head for `chain_id`, but only while this observer is still the
    /// wallet's current one, so a superseded observer cannot invalidate a live chain's cache.
    pub(super) fn invalidate_block_for_current_observer(
        &self,
        wallet_id: &str,
        active_wallet_generation: u64,
        chain_id: u64,
        observer_token: &InstalledObserverToken,
    ) {
        if self.is_current_installed_observer(
            wallet_id,
            active_wallet_generation,
            chain_id,
            observer_token,
        ) {
            self.http.rpc_broker().invalidate_block(chain_id);
        }
    }

    pub(super) fn transition_current_installed_observer_to_error(
        &mut self,
        wallet_id: &str,
        active_wallet_generation: u64,
        chain_id: u64,
        lifecycle_generation: u64,
        task_id: u64,
        observer_token: &InstalledObserverToken,
        ppoi_workflow_status: Option<WalletPpoiWorkflowStatus>,
        message: Arc<str>,
        cx: &mut Context<'_, Self>,
    ) -> Option<Arc<wallet_ops::WalletSession>> {
        let terminal = installed_observer_terminal_transition(
            self.selected_wallet_id.as_deref(),
            self.active_wallet_generation,
            wallet_id,
            active_wallet_generation,
            chain_id,
            self.chain_states
                .get(&chain_id)
                .and_then(|state| match state {
                    ChainUtxoState::Syncing {
                        session,
                        observer_token,
                        ..
                    } => Some(InstalledObserverProjection::Syncing {
                        token: observer_token,
                        start_block: session.start_block,
                    }),
                    ChainUtxoState::Ready {
                        session,
                        observer_token,
                        ..
                    } => Some(InstalledObserverProjection::Ready {
                        token: observer_token,
                        start_block: session.start_block,
                    }),
                    ChainUtxoState::Idle
                    | ChainUtxoState::Loading { .. }
                    | ChainUtxoState::Error { .. } => None,
                }),
            observer_token,
            message,
        )?;
        self.http.rpc_broker().invalidate_block(chain_id);
        let state = self.chain_states.remove(&chain_id)?;
        let (session, previous_ppoi_workflow_status) = match state {
            ChainUtxoState::Syncing {
                session,
                ppoi_workflow_status,
                ..
            }
            | ChainUtxoState::Ready {
                session,
                ppoi_workflow_status,
                ..
            } => (session, ppoi_workflow_status),
            state @ (ChainUtxoState::Idle
            | ChainUtxoState::Loading { .. }
            | ChainUtxoState::Error { .. }) => {
                self.chain_states.insert(chain_id, state);
                return None;
            }
        };
        self.finish_chain_load_startup(chain_id, lifecycle_generation, task_id);
        self.handle_initial_sync_observation(
            active_wallet_generation,
            chain_id,
            InitialSyncObservation::Error,
        );
        self.chain_states.insert(
            chain_id,
            ChainUtxoState::Error {
                message: terminal.message,
                start_block: Some(terminal.start_block),
                ppoi_workflow_status: ppoi_workflow_status.unwrap_or(previous_ppoi_workflow_status),
            },
        );
        if self.last_evm_chain == chain_id {
            self.sync_utxo_table(cx);
        }
        cx.notify();
        Some(session)
    }

    pub(super) fn finish_chain_load_startup(
        &mut self,
        chain_id: u64,
        lifecycle_generation: u64,
        task_id: u64,
    ) {
        self.wallet_sync_lifecycle
            .finish_startup(chain_id, lifecycle_generation, task_id);
    }
}
