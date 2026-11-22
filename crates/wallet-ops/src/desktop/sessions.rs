use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex as AsyncMutex, RwLock as AsyncRwLock};

use super::*;

const WALLET_SYNC_TIP_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalletSyncTip {
    pub last_scanned_block: Option<u64>,
    pub head_block: Option<u64>,
    pub safe_head_block: Option<u64>,
    pub head_last_advanced_at_unix_secs: Option<u64>,
    pub indexed_catch_up: Option<WalletIndexedCatchUpStatus>,
}

#[derive(Clone)]
pub struct WalletSessionObservation {
    pub snapshot: Arc<ListUtxosOutput>,
    pub readiness: WalletReadiness,
    pub ppoi_workflow_status: WalletPpoiWorkflowStatus,
}

pub struct WalletSession {
    pub chain_id: u64,
    pub poi_read_source: PoiReadSource,
    pub cache_key: String,
    pub start_block: u64,
    pub observation_rx: watch::Receiver<WalletSessionObservation>,
    pub sync_tip_rx: watch::Receiver<WalletSyncTip>,
    pub poi_refreshing_rx: watch::Receiver<bool>,
    pub poi_artifact_cache_progress_rx:
        Option<watch::Receiver<BTreeMap<u64, PoiArtifactCacheProgress>>>,
    pub(crate) db: Arc<DbStore>,
    pub(crate) sync_manager: Arc<SyncManager>,
    pub(crate) chain_key: ChainKey,
    pub(crate) handle: WalletHandle,
    pub(crate) public_data_plane: PublicDataPlaneHandle,
    pub(super) projection_cancel_tx: watch::Sender<bool>,
    pub(super) projection_join: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(super) executor_owner: Option<Arc<ExecutorOwner>>,
}

pub struct WalletPoiArtifactCacheRetry {
    retry: CorePoiArtifactCacheRetry,
    handle: WalletHandle,
}

impl WalletPoiArtifactCacheRetry {
    #[must_use]
    pub const fn attempt_id(&self) -> PoiArtifactCacheAttemptId {
        self.retry.attempt_id()
    }

    pub async fn wait(self) -> Result<bool> {
        self.retry
            .wait()
            .await
            .wrap_err("retry chain PPOI corpus refresh")?;
        Ok(self.handle.refresh_poi_statuses().await)
    }
}

impl WalletSession {
    #[must_use]
    pub fn executor_owner(&self) -> Option<Arc<ExecutorOwner>> {
        self.executor_owner.clone()
    }

    pub async fn stop(&self) -> Result<()> {
        if let Some(owner) = &self.executor_owner {
            owner.shutdown().await;
        }
        let result = self
            .sync_manager
            .remove_wallet_session(&self.handle)
            .await
            .wrap_err("remove wallet sync worker");
        if result.is_err() {
            let _ = self.projection_cancel_tx.send(true);
        }
        let projection_join = match self.projection_join.lock() {
            Ok(mut projection_join) => projection_join.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(projection_join) = projection_join {
            projection_join
                .await
                .wrap_err("join wallet session observation projection")?;
        }
        result
    }

    #[must_use]
    pub fn unspent_utxos(&self) -> Vec<Utxo> {
        let Some(snapshot) = self.handle.current_snapshot() else {
            return Vec::new();
        };
        let inputs =
            poi_verified_unspent_utxos_from_records(&snapshot.utxos, &snapshot.pending_overlay);
        if let Some(owner) = &self.executor_owner {
            return owner.available_inputs(inputs).unwrap_or_else(|_| {
                tracing::warn!(
                    "executor input reservations are unavailable; private spending remains blocked"
                );
                Vec::new()
            });
        }
        inputs
    }

    /// Spendable notes for this prepared operation, including its own durable reservation.
    pub fn unspent_utxos_for_executor(
        &self,
        prepared: &PreparedExecutorOperation,
    ) -> Result<Vec<Utxo>> {
        let snapshot = self
            .handle
            .current_snapshot()
            .ok_or_else(|| eyre!("private wallet snapshot is unavailable"))?;
        // Retain ordinary actor/chain pending-spend and POI admission. Only this
        // operation's additional durable executor reservation may be reused.
        let inputs =
            poi_verified_unspent_utxos_from_records(&snapshot.utxos, &snapshot.pending_overlay);
        self.executor_owner
            .as_ref()
            .ok_or_else(|| eyre!("executor wallet ownership is unavailable"))?
            .inputs_for_preparation(inputs, prepared)
    }

    /// Spendable notes for a later preparation of `operation`, including its own durable
    /// reservation, as [`Self::unspent_utxos_for_executor`] admits them once it is prepared.
    pub fn unspent_utxos_for_operation(
        &self,
        operation: vault::ExecutorOperationId,
    ) -> Result<Vec<Utxo>> {
        let snapshot = self
            .handle
            .current_snapshot()
            .ok_or_else(|| eyre!("private wallet snapshot is unavailable"))?;
        let inputs =
            poi_verified_unspent_utxos_from_records(&snapshot.utxos, &snapshot.pending_overlay);
        let owner = self
            .executor_owner
            .as_ref()
            .ok_or_else(|| eyre!("executor wallet ownership is unavailable"))?;
        // An operation not yet recorded reserves nothing of its own.
        match owner
            .records()?
            .into_iter()
            .find(|record| record.operation() == operation)
        {
            Some(record) => owner.inputs_for_recovery(inputs, &record),
            None => owner.available_inputs(inputs),
        }
    }

    pub(crate) async fn mark_pending_spent_utxos(
        &self,
        utxos: &[Utxo],
        tx_hash: Option<FixedBytes<32>>,
    ) {
        match self.handle.mark_pending_spent_utxos(utxos, tx_hash).await {
            Ok(
                WalletPendingSpentMarkOutcome::Marked
                | WalletPendingSpentMarkOutcome::AlreadyProtected,
            ) => {}
            Err(error) => {
                tracing::warn!(%error, "wallet actor rejected local pending-spend update");
            }
        }
    }

    pub(crate) async fn renew_pending_spent_utxos(
        &self,
        utxos: &[Utxo],
        tx_hash: FixedBytes<32>,
    ) -> Result<()> {
        self.handle
            .mark_pending_spent_utxos(utxos, Some(tx_hash))
            .await
            .map(|_| ())
            .map_err(Report::new)
            .wrap_err("renew local pending-spend protection")
    }

    pub async fn clear_local_pending_spent(&self) -> bool {
        self.handle
            .clear_all_local_pending_spent()
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "wallet actor rejected local pending-spend clear");
                false
            })
    }

    pub async fn refresh_poi_statuses(&self) -> bool {
        self.handle.refresh_poi_statuses().await
    }

    pub async fn retry_poi_artifact_cache(&self) -> Result<WalletPoiArtifactCacheRetry> {
        let retry = self
            .public_data_plane
            .retry_poi_artifact_cache()
            .await
            .wrap_err("retry chain PPOI corpus refresh")?;
        Ok(WalletPoiArtifactCacheRetry {
            retry,
            handle: self.handle.clone(),
        })
    }

    /// This chain's amount statistics at `now`, or why they are unavailable. Makes no request.
    #[must_use]
    pub fn amount_statistics_status(
        &self,
        chains: &settings::EffectiveChainRegistry,
        now: SystemTime,
    ) -> AmountStatisticsStatus {
        amount_statistics_status_for_chain(chains.get(self.chain_id), || {
            self.sync_manager
                .amount_statistics_status(&self.chain_key, now)
        })
    }

    /// The wallet's own deposits of `token`: its notes received from a shield, spent or not.
    /// Makes no request.
    #[must_use]
    pub fn own_shield_deposits(&self, token: Address) -> Vec<crate::OwnDeposit> {
        self.handle
            .current_snapshot()
            .map_or_else(Vec::new, |snapshot| {
                crate::own_shield_deposits(&snapshot.utxos, token)
            })
    }

    /// The highest commitment tree in the chain's Merkle forest, the one new deposits enter
    /// once the forest is synced. `None` while the chain isn't running or its forest is being
    /// written. Makes no request.
    #[must_use]
    pub fn merkle_forest_tip_tree(&self) -> Option<u32> {
        // `chain_handle` only reads a std mutex, so it is ready when first polled.
        let chain = futures_util::FutureExt::now_or_never(
            self.sync_manager.chain_handle(&self.chain_key),
        )??;
        let forest = chain.forest.try_read().ok()?;
        forest
            .computed_roots()
            .last_key_value()
            .map(|(tree, _)| *tree)
    }
}

/// Settings decide first, so a chain without a statistics source never asks the sync manager.
/// `running_chain_status` is `None` while the chain has no session.
#[must_use]
pub fn amount_statistics_status_for_chain(
    chain: Option<&settings::EffectiveChainConfig>,
    running_chain_status: impl FnOnce() -> Option<AmountStatisticsStatus>,
) -> AmountStatisticsStatus {
    let configured = chain
        .and_then(|chain| chain.railgun.as_ref())
        .and_then(|private| private.sync.indexed_artifact_source.as_ref())
        .is_some_and(|source| source.amount_statistics_ipns_name.is_some());
    if !configured {
        return AmountStatisticsStatus::NotConfigured;
    }
    // A chain that is not running yet holds no accepted snapshot.
    running_chain_status().unwrap_or(AmountStatisticsStatus::NotYetAvailable)
}

impl Drop for WalletSession {
    fn drop(&mut self) {
        if let Some(owner) = &self.executor_owner {
            owner.close();
        }
        let _ = self.projection_cancel_tx.send(true);
        if let Ok(projection_join) = self.projection_join.get_mut()
            && let Some(projection_join) = projection_join.take()
        {
            projection_join.abort();
        }
    }
}

pub struct WalletSessionStore {
    db: Arc<DbStore>,
    sync_manager: Arc<SyncManager>,
    active_wallet_scope: AsyncMutex<ActiveWalletScope>,
    wallet_startups: AsyncRwLock<()>,
    executor_owners: Mutex<ExecutorOwners>,
}

#[derive(Default)]
struct ExecutorOwners {
    minimum_generation: u64,
    closing: bool,
    owners: Vec<Arc<ExecutorOwner>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PersistedPoiDataResetReport {
    pub data_records_removed: u64,
    pub chunk_records_removed: u64,
}

impl PersistedPoiDataResetReport {
    #[must_use]
    pub const fn total_removed_entries(self) -> u64 {
        self.data_records_removed
            .saturating_add(self.chunk_records_removed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("failed to reset {failed_resource} after removing {removed_entries} entries: {reason}")]
pub struct PersistedPoiDataResetError {
    pub kind: PersistedPublicSyncCacheKind,
    pub reason: String,
    pub partial_report: PersistedPoiDataResetReport,
    failed_resource: &'static str,
    removed_entries: u64,
}

impl From<PersistedPublicSyncCacheResetError> for PersistedPoiDataResetError {
    fn from(error: PersistedPublicSyncCacheResetError) -> Self {
        let partial_report = PersistedPoiDataResetReport {
            data_records_removed: error.poi_corpus_entries_removed,
            chunk_records_removed: error
                .partial_report
                .poi_artifact_checkpoint_chunk_entries_removed,
        };
        let failed_resource = match error.kind {
            PersistedPublicSyncCacheKind::PoiCorpus => "verified PPOI data",
            PersistedPublicSyncCacheKind::PoiArtifactCheckpointChunks => "downloaded PPOI chunks",
            PersistedPublicSyncCacheKind::Txid
            | PersistedPublicSyncCacheKind::WalletScanArtifactChunks
            | PersistedPublicSyncCacheKind::AmountStatistics => "PPOI data",
        };
        Self {
            kind: error.kind,
            reason: error.reason,
            removed_entries: partial_report.total_removed_entries(),
            partial_report,
            failed_resource,
        }
    }
}

/// Resets PPOI data after every wallet sync owner for `db` has shut down.
pub async fn reset_persisted_poi_data(
    db: &DbStore,
) -> Result<PersistedPoiDataResetReport, PersistedPoiDataResetError> {
    let reset = sync_service::reset_offline_poi_corpus(db)
        .await
        .map_err(PersistedPoiDataResetError::from)?;
    Ok(PersistedPoiDataResetReport {
        data_records_removed: reset.corpus_entries_removed,
        chunk_records_removed: reset.raw_chunk_entries_removed,
    })
}

#[derive(Default)]
struct ActiveWalletScope {
    generation: u64,
    wallet_id: Option<String>,
}

impl ActiveWalletScope {
    fn requires_replacement(&self, generation: u64, wallet_id: &str) -> Result<bool> {
        if generation < self.generation {
            return Err(eyre!(
                "wallet session scope generation {generation} was superseded by {}",
                self.generation
            ));
        }
        if generation == self.generation
            && self
                .wallet_id
                .as_deref()
                .is_some_and(|active_wallet_id| active_wallet_id != wallet_id)
        {
            return Err(eyre!(
                "wallet session scope generation {generation} is already owned by another wallet"
            ));
        }
        Ok(generation > self.generation || self.wallet_id.as_deref() != Some(wallet_id))
    }

    fn replace(&mut self, generation: u64, wallet_id: String) {
        self.generation = generation;
        self.wallet_id = Some(wallet_id);
    }
}

impl WalletSessionStore {
    pub fn open(db_path: PathBuf, poi_policy: PoiReadSource) -> Result<Self> {
        let db = Arc::new(DbStore::open(DbConfig { root_dir: db_path }).wrap_err("open local db")?);
        Self::from_db(db, poi_policy)
    }

    pub fn from_db(db: Arc<DbStore>, poi_policy: PoiReadSource) -> Result<Self> {
        let sync_manager = Arc::new(
            SyncManager::new(Arc::clone(&db), poi_policy)
                .wrap_err("acquire sync manager database ownership")?,
        );

        Ok(Self {
            db,
            sync_manager,
            active_wallet_scope: AsyncMutex::new(ActiveWalletScope::default()),
            wallet_startups: AsyncRwLock::new(()),
            executor_owners: Mutex::new(ExecutorOwners::default()),
        })
    }

    pub async fn start_view_wallet_session(
        &self,
        request: ViewWalletChainSessionRequest,
        http: &HttpContext,
    ) -> Result<WalletSession> {
        Box::pin(self.start_view_wallet_session_with_wait(request, http, true)).await
    }

    pub async fn start_view_wallet_session_immediate(
        &self,
        request: ViewWalletChainSessionRequest,
        http: &HttpContext,
    ) -> Result<WalletSession> {
        Box::pin(self.start_view_wallet_session_with_wait(request, http, false)).await
    }

    pub async fn reset_public_sync_caches(&self) -> Result<PublicSyncCachesResetReport> {
        self.sync_manager
            .reset_public_sync_caches()
            .await
            .wrap_err("reset public sync caches")
    }

    async fn start_view_wallet_session_with_wait(
        &self,
        request: ViewWalletChainSessionRequest,
        http: &HttpContext,
        wait_until_ready: bool,
    ) -> Result<WalletSession> {
        // Reject unavailable/private-ineligible requests before replacing any active owner.
        settings::resolve_effective_chain_rpc_route(request.chain_id, &request.effective_chain)?;
        request.effective_chain.require_railgun()?;
        self.sync_manager.set_gateway_pool(http.gateway_pool());
        let wallet_id = request.view_session.wallet_id().to_owned();
        let mut active_scope = self.active_wallet_scope.lock().await;
        if active_scope.requires_replacement(request.wallet_scope_generation, &wallet_id)? {
            // Replacement must not remove actors while an admitted startup still creates them.
            let _exclusive_startups = self.wallet_startups.write().await;
            self.invalidate_executor_owners(request.wallet_scope_generation);
            self.shutdown_superseded_executor_owners(request.wallet_scope_generation)
                .await;
            self.sync_manager.remove_all_wallets().await;
            active_scope.replace(request.wallet_scope_generation, wallet_id);
        }
        // Admit this startup while the scope is locked, then let other chains in the same
        // scope start concurrently. The shared guard prevents replacement until it finishes.
        let _startup = self.wallet_startups.read().await;
        drop(active_scope);

        let chain_id = request.chain_id;
        let executor_view = Arc::clone(&request.view_session);
        let executor_chain = request.effective_chain.clone();
        let executor_generation = request.wallet_scope_generation;
        let synced = setup_synced_view_wallet_with_store(
            request.view_session,
            chain_id,
            request.sync_start_policy,
            request.init_block_number,
            request.sync_to_block,
            request.use_indexed_wallet_catch_up,
            request.effective_chain.clone(),
            request.poi_read_source.clone(),
            request.rewind_wallet_cache,
            http,
            request.progress_tx.clone(),
            wait_until_ready,
            Arc::clone(&self.db),
            Arc::clone(&self.sync_manager),
        )
        .await?;

        let mut session =
            wallet_session_from_view_synced(chain_id, request.poi_read_source, synced).await?;
        let chain = executor_chain;
        if chain.chain_id != chain_id {
            session.stop().await?;
            return Err(eyre!(
                "executor configuration does not match the wallet chain"
            ));
        }
        let owner =
            self.create_executor_owner(executor_generation, executor_view, chain, http.clone());
        match owner {
            Ok(owner) => {
                owner.start_tip_observation(session.sync_tip_rx.clone());
                owner.start_confirmation_observation(
                    session.handle.clone(),
                    session.sync_tip_rx.clone(),
                );
                session.executor_owner = Some(owner);
            }
            Err(error) => {
                session.stop().await?;
                return Err(error);
            }
        }
        Ok(session)
    }

    /// Called synchronously by the desktop lifecycle before it starts asynchronous cleanup.
    pub fn invalidate_executor_owners(&self, minimum_generation: u64) {
        let mut owners = self
            .executor_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        owners.minimum_generation = owners.minimum_generation.max(minimum_generation);
        for owner in &owners.owners {
            if owner.generation() < owners.minimum_generation {
                owner.close();
            }
        }
    }

    fn create_executor_owner(
        &self,
        generation: u64,
        view: Arc<vault::DesktopViewSession>,
        chain: settings::EffectiveChainConfig,
        http: HttpContext,
    ) -> Result<Arc<ExecutorOwner>> {
        // Opening the encrypted store can perform I/O. Keep it outside the
        // registry lock used by synchronous desktop lifecycle invalidation.
        let owner = Arc::new(ExecutorOwner::new(
            generation,
            self.db.clone(),
            view,
            chain,
            http,
        )?);
        let mut owners = self
            .executor_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if owners.closing || generation < owners.minimum_generation {
            owner.close();
            return Err(eyre!("executor wallet session was superseded"));
        }
        owners.owners.push(owner.clone());
        Ok(owner)
    }

    async fn shutdown_superseded_executor_owners(&self, generation: u64) {
        let retired = {
            let owners = self
                .executor_owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            owners
                .owners
                .iter()
                .filter(|owner| owner.generation() < generation)
                .cloned()
                .collect::<Vec<_>>()
        };
        for owner in retired {
            owner.shutdown().await;
        }
        // Keep owners registered until cleanup completes, including if this future is cancelled.
        self.executor_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .owners
            .retain(|owner| owner.generation() >= generation);
    }

    pub async fn shutdown(&self) {
        let owners = {
            let mut owners = self
                .executor_owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            owners.closing = true;
            for owner in &owners.owners {
                owner.close();
            }
            owners.owners.clone()
        };
        for owner in owners {
            owner.shutdown().await;
        }
        self.executor_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .owners
            .clear();
        self.sync_manager.shutdown().await;
    }
}

async fn wallet_session_from_view_synced(
    chain_id: u64,
    poi_read_source: PoiReadSource,
    synced: SyncedViewWallet,
) -> Result<WalletSession> {
    wallet_session_from_parts(
        chain_id,
        poi_read_source,
        synced.db,
        synced.sync_manager,
        synced.chain_key,
        synced.start_block,
        synced.handle,
        synced.public_data_plane,
    )
    .await
}

async fn wallet_session_from_parts(
    chain_id: u64,
    poi_read_source: PoiReadSource,
    db: Arc<DbStore>,
    sync_manager: Arc<SyncManager>,
    chain_key: ChainKey,
    start_block: u64,
    handle: WalletHandle,
    public_data_plane: PublicDataPlaneHandle,
) -> Result<WalletSession> {
    let mut core_observation_rx = handle.subscribe_observation();
    let core_observation = core_observation_rx.borrow_and_update().clone();
    let initial_observation =
        wallet_session_observation(chain_id, &handle.cache_key, &core_observation)?;
    let (observation_tx, observation_rx) = watch::channel(initial_observation);
    let (projection_cancel_tx, mut projection_cancel_rx) = watch::channel(false);
    let snapshot_cache_key = handle.cache_key.clone();
    let projection_join = tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = core_observation_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let core_observation = core_observation_rx.borrow_and_update().clone();
                    let terminal = core_observation.readiness() == &WalletReadiness::Shutdown;
                    let observation = wallet_session_observation(
                        chain_id,
                        &snapshot_cache_key,
                        &core_observation,
                    )
                    .expect("published wallet observation satisfies projection invariants");
                    if observation_tx.send(observation).is_err() || terminal {
                        break;
                    }
                }
                changed = projection_cancel_rx.changed() => {
                    if changed.is_err() || *projection_cancel_rx.borrow() {
                        break;
                    }
                }
            }
        }
    });
    let cache_key = handle.cache_key.to_string();
    let sync_tip_rx =
        spawn_wallet_sync_tip_task(handle.clone(), sync_manager.chain_handle(&chain_key).await);
    let poi_refreshing_rx = handle.poi_refreshing_rx.clone();
    let poi_artifact_cache_progress_rx = public_data_plane.poi_artifact_cache_progress_rx();

    Ok(WalletSession {
        chain_id,
        poi_read_source,
        cache_key,
        start_block,
        observation_rx,
        sync_tip_rx,
        poi_refreshing_rx,
        poi_artifact_cache_progress_rx,
        db,
        sync_manager,
        chain_key,
        handle,
        public_data_plane,
        projection_cancel_tx,
        projection_join: Mutex::new(Some(projection_join)),
        executor_owner: None,
    })
}

fn wallet_session_observation(
    chain_id: u64,
    cache_key: &str,
    observation: &WalletObservation,
) -> Result<WalletSessionObservation> {
    let snapshot = Arc::new(match observation.view() {
        WalletViewState::Current(_) => snapshot_from_view(
            chain_id,
            cache_key,
            observation.view(),
            observation.ppoi_submission_statuses(),
        )
        .expect("observed current wallet view contains a snapshot"),
        WalletViewState::ResetPending { .. } => empty_wallet_snapshot(chain_id, cache_key),
        WalletViewState::Inactive { reason, .. } => {
            if observation.readiness() != &WalletReadiness::Shutdown {
                return Err(eyre!(
                    "inactive wallet observation is not shut down: {reason:?}"
                ));
            }
            empty_wallet_snapshot(chain_id, cache_key)
        }
    });
    Ok(wallet_session_observation_from_parts(
        snapshot,
        observation.readiness().clone(),
        *observation.ppoi_workflow_status(),
    ))
}

const fn wallet_session_observation_from_parts(
    snapshot: Arc<ListUtxosOutput>,
    readiness: WalletReadiness,
    ppoi_workflow_status: WalletPpoiWorkflowStatus,
) -> WalletSessionObservation {
    WalletSessionObservation {
        snapshot,
        readiness,
        ppoi_workflow_status,
    }
}

fn empty_wallet_snapshot(chain_id: u64, cache_key: &str) -> ListUtxosOutput {
    ListUtxosOutput {
        chain_id,
        cache_key: cache_key.to_string(),
        utxo_count: 0,
        unspent_count: 0,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: Vec::new(),
        totals: Vec::new(),
    }
}

fn spawn_wallet_sync_tip_task(
    handle: WalletHandle,
    chain_handle: Option<sync_service::ChainHandle>,
) -> watch::Receiver<WalletSyncTip> {
    let now = now_epoch_secs();
    let head_block = chain_handle
        .as_ref()
        .map_or(0, |chain| *chain.head_rx.borrow());
    let safe_head_block = chain_handle
        .as_ref()
        .map_or(0, |chain| *chain.safe_head_rx.borrow());
    let head_last_advanced_at_unix_secs = nonzero_block(head_block).map(|_| now);
    let indexed_catch_up = *handle.indexed_catch_up_rx.borrow();
    let initial_tip = wallet_sync_tip_from_blocks(
        handle.last_scanned(),
        head_block,
        safe_head_block,
        head_last_advanced_at_unix_secs,
        indexed_catch_up,
    );
    let (sync_tip_tx, sync_tip_rx) = watch::channel(initial_tip);

    if let Some(chain_handle) = chain_handle {
        spawn_wallet_sync_tip_with_chain(
            handle,
            chain_handle,
            sync_tip_tx,
            initial_tip,
            head_block,
            head_last_advanced_at_unix_secs,
        );
    } else {
        spawn_wallet_sync_tip_without_chain(handle, sync_tip_tx, initial_tip);
    }

    sync_tip_rx
}

fn spawn_wallet_sync_tip_with_chain(
    handle: WalletHandle,
    chain_handle: sync_service::ChainHandle,
    sync_tip_tx: watch::Sender<WalletSyncTip>,
    mut last_tip: WalletSyncTip,
    mut max_observed_head_block: u64,
    mut head_last_advanced_at_unix_secs: Option<u64>,
) {
    let mut head_rx = chain_handle.head_rx;
    let mut safe_head_rx = chain_handle.safe_head_rx;
    let mut indexed_catch_up_rx = handle.indexed_catch_up_rx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(WALLET_SYNC_TIP_REFRESH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let force_send = tokio::select! {
                _ = interval.tick() => true,
                changed = head_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    false
                }
                changed = safe_head_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    false
                }
                changed = indexed_catch_up_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    false
                }
            };

            let head_block = *head_rx.borrow();
            update_head_advance_state(
                &mut max_observed_head_block,
                &mut head_last_advanced_at_unix_secs,
                head_block,
                now_epoch_secs(),
            );
            let tip = wallet_sync_tip_from_blocks(
                handle.last_scanned(),
                head_block,
                *safe_head_rx.borrow(),
                head_last_advanced_at_unix_secs,
                *indexed_catch_up_rx.borrow(),
            );
            if !publish_wallet_sync_tip(&sync_tip_tx, &mut last_tip, tip, force_send) {
                break;
            }
        }
    });
}

fn spawn_wallet_sync_tip_without_chain(
    handle: WalletHandle,
    sync_tip_tx: watch::Sender<WalletSyncTip>,
    mut last_tip: WalletSyncTip,
) {
    let mut indexed_catch_up_rx = handle.indexed_catch_up_rx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(WALLET_SYNC_TIP_REFRESH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let force_send = tokio::select! {
                _ = interval.tick() => true,
                changed = indexed_catch_up_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    false
                }
            };
            let tip = WalletSyncTip {
                last_scanned_block: handle.last_scanned(),
                indexed_catch_up: *indexed_catch_up_rx.borrow(),
                ..WalletSyncTip::default()
            };
            if !publish_wallet_sync_tip(&sync_tip_tx, &mut last_tip, tip, force_send) {
                break;
            }
        }
    });
}

fn publish_wallet_sync_tip(
    sync_tip_tx: &watch::Sender<WalletSyncTip>,
    last_tip: &mut WalletSyncTip,
    tip: WalletSyncTip,
    force_send: bool,
) -> bool {
    if !force_send && tip == *last_tip {
        return true;
    }
    if sync_tip_tx.send(tip).is_err() {
        return false;
    }
    *last_tip = tip;
    true
}

const fn wallet_sync_tip_from_blocks(
    last_scanned_block: Option<u64>,
    head_block: u64,
    safe_head_block: u64,
    head_last_advanced_at_unix_secs: Option<u64>,
    indexed_catch_up: Option<WalletIndexedCatchUpStatus>,
) -> WalletSyncTip {
    WalletSyncTip {
        last_scanned_block,
        head_block: nonzero_block(head_block),
        safe_head_block: nonzero_block(safe_head_block),
        head_last_advanced_at_unix_secs,
        indexed_catch_up,
    }
}

const fn update_head_advance_state(
    max_observed_head_block: &mut u64,
    head_last_advanced_at_unix_secs: &mut Option<u64>,
    head_block: u64,
    now_secs: u64,
) {
    if head_block > *max_observed_head_block {
        *max_observed_head_block = head_block;
        *head_last_advanced_at_unix_secs = Some(now_secs);
    }
}

const fn nonzero_block(block: u64) -> Option<u64> {
    if block == 0 { None } else { Some(block) }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests;
