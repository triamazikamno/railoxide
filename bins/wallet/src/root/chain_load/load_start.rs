#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn ensure_chain_load(&mut self, chain_id: u64, cx: &mut Context<'_, Self>) {
        let overrides = chain_load_overrides();
        self.start_chain_load(chain_id, &overrides, false, cx);
    }

    pub(in crate::root) fn ensure_chain_load_with_start_policy(
        &mut self,
        chain_id: u64,
        sync_start_policy: Option<DesktopWalletSyncStartPolicy>,
        cx: &mut Context<'_, Self>,
    ) {
        let mut overrides = chain_load_overrides();
        overrides.sync_start_policy = sync_start_policy;
        self.start_chain_load(chain_id, &overrides, false, cx);
    }

    pub(in crate::root) fn start_chain_load(
        &mut self,
        chain_id: u64,
        overrides: &ChainLoadOverrides,
        force: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Ok(resolved_chain) = self.effective_chain_configs.railgun(chain_id).cloned() else {
            return;
        };
        if !chain_load_start_is_allowed(
            self.manage_wallets.deleting_wallet_id.as_deref(),
            self.selected_wallet_id.as_deref(),
        ) {
            tracing::debug!(target: LOG_TARGET, chain_id, "skipping wallet sync during wallet deletion");
            return;
        }
        if !wallet_sync_start_is_admitted(
            matches!(
                self.vault_state,
                crate::root::VaultState::PendingSoftwareProfileOpen
            ),
            self.view_session.is_some(),
            self.selected_wallet_id.is_some(),
            wallet_sync_maintenance_allows_start(
                self.public_sync_cache_resetting,
                self.merkle_forest_cache_resetting,
            ),
        ) {
            tracing::debug!(target: LOG_TARGET,
                chain_id,
                "skipping wallet sync without an admitted wallet context"
            );
            return;
        }
        let Some(view_session) = self.view_session.clone() else {
            return;
        };
        if matches!(
            self.chain_states.get(&chain_id),
            Some(
                ChainUtxoState::Loading { .. }
                    | ChainUtxoState::Syncing { .. }
                    | ChainUtxoState::Ready { .. }
            )
        ) && !force
        {
            return;
        }

        let previous_start_block = self
            .chain_states
            .get(&chain_id)
            .and_then(ChainUtxoState::start_block);

        let previous_session = if force {
            match self.chain_states.remove(&chain_id) {
                Some(
                    ChainUtxoState::Syncing { session, .. } | ChainUtxoState::Ready { session, .. },
                ) => Some(session),
                Some(state) => {
                    self.chain_states.insert(chain_id, state);
                    None
                }
                None => None,
            }
        } else {
            None
        };

        self.chain_states.insert(
            chain_id,
            ChainUtxoState::Loading {
                progress: ChainSyncProgress::default(),
            },
        );
        self.handle_initial_sync_observation(
            self.active_wallet_generation,
            chain_id,
            InitialSyncObservation::Started,
        );
        self.sync_utxo_table(cx);

        let active_wallet_id: Arc<str> = Arc::from(view_session.wallet_id().to_owned());
        let active_wallet_generation = self.active_wallet_generation;
        let (progress_tx, progress_rx) = watch::channel(None);
        let registration = self.wallet_sync_lifecycle.prepare_startup(chain_id);
        let lifecycle_generation = registration.generation;
        let scope = ChainLoadScope {
            chain_id,
            wallet_id: active_wallet_id,
            wallet_generation: active_wallet_generation,
            lifecycle_generation,
            task_id: registration.task_id,
            observer_token: registration.observer_token.clone(),
        };
        let lifecycle_generation_token = Arc::clone(&registration.generation_token);
        let session_store = Arc::clone(&registration.session_store);
        let request = ViewWalletChainSessionRequest {
            view_session,
            wallet_scope_generation: lifecycle_generation,
            chain_id,
            effective_chain: resolved_chain,
            sync_start_policy: overrides
                .sync_start_policy
                .unwrap_or_else(|| self.selected_wallet_sync_start_policy()),
            init_block_number: overrides.init_block_number,
            sync_to_block: overrides.sync_to_block,
            use_indexed_wallet_catch_up: overrides.use_indexed_wallet_catch_up,
            poi_read_source: self.poi_read_source.clone(),
            rewind_wallet_cache: overrides.rewind_wallet_cache,
            progress_tx: Some(progress_tx),
        };
        let db_path = self.options.db_path.clone();
        let http = self.http.clone();
        let poi_read_source = self.poi_read_source.clone();
        let vault_db = self.vault_store.as_ref().map(|store| store.db());
        self.prune_finished_wallet_sync_cleanups();
        let pending_cleanups = self.wallet_sync_cleanup_tasks.clone();
        let (result_tx, result_rx) = oneshot::channel();
        let join = self.runtime.spawn(async move {
            let result = Box::pin(async move {
                if wallet_sync_startup_superseded(&lifecycle_generation_token, lifecycle_generation)
                {
                    return Err(wallet_sync_startup_superseded_error());
                }
                if let Some(previous_session) = previous_session {
                    previous_session.stop().await?;
                }
                if wallet_sync_startup_superseded(&lifecycle_generation_token, lifecycle_generation)
                {
                    return Err(wallet_sync_startup_superseded_error());
                }
                let store = session_store
                    .get_or_try_init(|| {
                        let db_path = db_path.clone();
                        let vault_db = vault_db.clone();
                        async move {
                            // A store from before the last lock may still own the database.
                            wait_for_wallet_sync_cleanups(pending_cleanups).await;
                            Ok::<Arc<WalletSessionStore>, eyre::Report>(Arc::new(match vault_db {
                                Some(db) => {
                                    WalletSessionStore::from_db(db, poi_read_source.clone())?
                                }
                                None => WalletSessionStore::open(db_path, poi_read_source.clone())?,
                            }))
                        }
                    })
                    .await?
                    .clone();
                if wallet_sync_startup_superseded(&lifecycle_generation_token, lifecycle_generation)
                {
                    return Err(wallet_sync_startup_superseded_error());
                }
                let session =
                    Box::pin(store.start_view_wallet_session_immediate(request, &http)).await?;
                if wallet_sync_startup_superseded(&lifecycle_generation_token, lifecycle_generation)
                {
                    if let Err(error) = session.stop().await {
                        tracing::warn!(target: LOG_TARGET,
                            chain_id,
                            %error,
                            "failed to stop superseded wallet sync session"
                        );
                    }
                    return Err(wallet_sync_startup_superseded_error());
                }
                Ok(session)
            })
            .await;
            let _ = result_tx.send(result);
        });
        self.wallet_sync_lifecycle
            .track_startup(&registration, join);

        Self::spawn_chain_progress_forwarder(scope.clone(), progress_rx, cx);

        let observer_task = self
            .wallet_sync_lifecycle
            .register_installed_observer(&registration);
        Self::spawn_chain_session_observer(
            scope,
            previous_start_block,
            result_rx,
            observer_task.cancel_rx,
            observer_task.completed_tx,
            cx,
        );
    }
}
