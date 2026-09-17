use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex as AsyncMutex;

use super::*;

const WALLET_SYNC_TIP_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

pub struct WalletSessionStore {
    db: Arc<DbStore>,
    sync_manager: Arc<SyncManager>,
    active_wallet_scope: AsyncMutex<ActiveWalletScope>,
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
            | PersistedPublicSyncCacheKind::WalletScanArtifactChunks => "PPOI data",
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
        })
    }

    pub async fn start_view_wallet_session(
        &self,
        request: ViewWalletChainSessionRequest,
        rpc_url_override: Option<Url>,
        http: &HttpContext,
    ) -> Result<WalletSession> {
        self.start_view_wallet_session_with_wait(request, rpc_url_override, http, true)
            .await
    }

    pub async fn start_view_wallet_session_immediate(
        &self,
        request: ViewWalletChainSessionRequest,
        rpc_url_override: Option<Url>,
        http: &HttpContext,
    ) -> Result<WalletSession> {
        self.start_view_wallet_session_with_wait(request, rpc_url_override, http, false)
            .await
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
        rpc_url_override: Option<Url>,
        http: &HttpContext,
        wait_until_ready: bool,
    ) -> Result<WalletSession> {
        self.sync_manager.set_gateway_pool(http.gateway_pool());
        let wallet_id = request.view_session.wallet_id().to_owned();
        let mut active_scope = self.active_wallet_scope.lock().await;
        if active_scope.requires_replacement(request.wallet_scope_generation, &wallet_id)? {
            self.sync_manager.remove_all_wallets().await;
            active_scope.replace(request.wallet_scope_generation, wallet_id);
        }

        let chain_id = request.chain_id;
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
            rpc_url_override,
            http,
            request.progress_tx.clone(),
            wait_until_ready,
            Arc::clone(&self.db),
            Arc::clone(&self.sync_manager),
        )
        .await?;

        wallet_session_from_view_synced(chain_id, request.poi_read_source, synced).await
    }

    pub async fn shutdown(&self) {
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
mod tests {
    use super::*;

    #[tokio::test]
    async fn offline_poi_data_reset_accepts_database_without_active_owners() {
        let root_dir = std::env::temp_dir().join(format!(
            "wallet-ops-offline-poi-reset-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        let db = DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open local DB");

        let report = reset_persisted_poi_data(&db)
            .await
            .expect("reset empty PPOI data");

        assert_eq!(report, PersistedPoiDataResetReport::default());
        drop(db);
        std::fs::remove_dir_all(root_dir).expect("remove local DB");
    }

    #[test]
    fn wallet_session_observation_keeps_ppoi_workflow_separate_from_snapshot() {
        let status = WalletPpoiWorkflowStatus {
            awaiting_recovery: 5,
            awaiting_public_txid_data: 0,
            awaiting_poi_data: 0,
            retrying_recovery: 0,
            recovery_needs_attention: 0,
            awaiting_submission: 1,
            awaiting_validation: 2,
            needs_attention: 3,
            validation_revision: 4,
        };
        let projected = wallet_session_observation_from_parts(
            Arc::new(empty_wallet_snapshot(1, "wallet")),
            WalletReadiness::Syncing,
            status,
        );

        assert_eq!(projected.ppoi_workflow_status, status);
        assert_eq!(projected.snapshot.utxo_count, 0);
    }

    #[test]
    fn active_wallet_scope_rejects_stale_and_same_generation_replacement() {
        let mut scope = ActiveWalletScope::default();
        assert!(
            scope
                .requires_replacement(0, "wallet-a")
                .expect("initial scope")
        );
        scope.replace(0, "wallet-a".to_string());
        assert!(
            !scope
                .requires_replacement(0, "wallet-a")
                .expect("same scope")
        );
        assert!(scope.requires_replacement(0, "wallet-b").is_err());
        assert!(
            scope
                .requires_replacement(1, "wallet-a")
                .expect("new generation of same wallet")
        );

        assert!(
            scope
                .requires_replacement(1, "wallet-b")
                .expect("new scope")
        );
        scope.replace(1, "wallet-b".to_string());
        assert!(scope.requires_replacement(0, "wallet-a").is_err());
    }

    #[test]
    fn head_advance_state_uses_monotonic_observed_head() {
        let mut max_observed_head_block = 100;
        let mut advanced_at = Some(10);

        update_head_advance_state(&mut max_observed_head_block, &mut advanced_at, 99, 20);
        assert_eq!(max_observed_head_block, 100);
        assert_eq!(advanced_at, Some(10));

        update_head_advance_state(&mut max_observed_head_block, &mut advanced_at, 100, 30);
        assert_eq!(max_observed_head_block, 100);
        assert_eq!(advanced_at, Some(10));

        update_head_advance_state(&mut max_observed_head_block, &mut advanced_at, 101, 40);
        assert_eq!(max_observed_head_block, 101);
        assert_eq!(advanced_at, Some(40));
    }

    #[test]
    fn wallet_sync_tip_publish_forces_time_driven_refresh() {
        let tip = WalletSyncTip {
            last_scanned_block: Some(100),
            head_block: Some(112),
            safe_head_block: Some(100),
            head_last_advanced_at_unix_secs: Some(10),
            indexed_catch_up: None,
        };
        let (tx, mut rx) = watch::channel(tip);
        let mut last_tip = tip;

        assert!(publish_wallet_sync_tip(&tx, &mut last_tip, tip, false));
        assert!(!rx.has_changed().expect("watch receiver open"));

        assert!(publish_wallet_sync_tip(&tx, &mut last_tip, tip, true));
        assert!(rx.has_changed().expect("watch receiver notified"));
        assert_eq!(*rx.borrow_and_update(), tip);

        let advanced_tip = WalletSyncTip {
            last_scanned_block: Some(101),
            ..tip
        };
        assert!(publish_wallet_sync_tip(
            &tx,
            &mut last_tip,
            advanced_tip,
            false,
        ));
        assert_eq!(last_tip, advanced_tip);
        assert!(rx.has_changed().expect("watch receiver notified"));
    }
}
