use super::*;
use eyre::eyre;

pub(super) fn snapshot_from_view(
    chain_id: u64,
    cache_key: &str,
    view: &WalletViewState,
    ppoi_submission_statuses: &[WalletPpoiSubmissionStatus],
) -> Option<ListUtxosOutput> {
    let snapshot = view.current_snapshot()?;
    let utxos = snapshot.utxos.to_vec();
    let pending_overlay = snapshot.pending_overlay.as_ref();
    let local_pending_spent_count = pending_overlay.local_pending_spent.len();
    let confirmed_utxos = utxos.clone();
    let (utxo_outputs, totals) = utxo_outputs_from_utxos(utxos);
    let mut utxo_outputs = utxo_outputs;
    apply_pending_overlay_to_outputs(&confirmed_utxos, pending_overlay.clone(), &mut utxo_outputs);
    apply_ppoi_submission_statuses(&mut utxo_outputs, ppoi_submission_statuses);
    let unspent_count = utxo_outputs.iter().filter(|utxo| !utxo.is_spent).count();
    let spent_count = utxo_outputs.len().saturating_sub(unspent_count);

    Some(ListUtxosOutput {
        chain_id,
        cache_key: cache_key.to_string(),
        utxo_count: utxo_outputs.len(),
        unspent_count,
        spent_count,
        local_pending_spent_count,
        utxos: utxo_outputs,
        totals,
    })
}

fn apply_ppoi_submission_statuses(
    outputs: &mut [UtxoOutput],
    statuses: &[WalletPpoiSubmissionStatus],
) {
    let timestamps = statuses
        .iter()
        .map(|status| {
            (
                hex::encode_prefixed(status.output_commitment),
                status.last_submission_at,
            )
        })
        .collect::<BTreeMap<_, _>>();

    for output in outputs {
        output.ppoi_last_submission_at = timestamps.get(&output.commitment).copied();
    }
}

pub(super) struct SyncedViewWallet {
    pub(super) db: Arc<DbStore>,
    pub(super) sync_manager: Arc<SyncManager>,
    pub(super) chain_key: ChainKey,
    pub(super) start_block: u64,
    pub(super) handle: WalletHandle,
    pub(super) public_data_plane: PublicDataPlaneHandle,
}

fn initialize_atomic_wallet_cache_metadata(
    db: &DbStore,
    cache_key: &WalletCacheKey,
    metadata: &vault::WalletChainMetadataBundle,
) -> Result<()> {
    db.put_wallet_meta_if_absent(
        cache_key,
        &WalletMeta {
            last_scanned_block: metadata.last_scanned_block,
            updated_at: 0,
            last_scanned_block_hash: metadata.last_scanned_block_hash,
        },
    )
    .wrap_err("initialize atomic wallet cache metadata")?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DesktopWalletChainStart {
    pub(crate) start_block: u64,
    pub(crate) last_scanned_block: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct NewWalletChainMetadataInitReport {
    pub(crate) initialized: usize,
    pub(crate) skipped_disabled: usize,
    pub(crate) skipped_unavailable: usize,
    pub(crate) skipped_selected: usize,
    pub(crate) skipped_existing: usize,
    pub(crate) failed: usize,
}

#[derive(Debug, Clone, Copy)]
enum NewWalletChainMetadataInitOutcome {
    Initialized,
    SkippedExisting,
    Failed,
}

#[must_use]
pub(crate) const fn new_wallet_chain_start_from_deployment(
    deployment_block: u64,
) -> DesktopWalletChainStart {
    DesktopWalletChainStart {
        start_block: deployment_block,
        last_scanned_block: deployment_block.saturating_sub(1),
    }
}

#[must_use]
pub(crate) const fn new_wallet_chain_start_from_head(
    deployment_block: u64,
    finality_depth: u64,
    head: u64,
) -> DesktopWalletChainStart {
    let finalized_head = head.saturating_sub(finality_depth);
    let safe_head = if finalized_head > deployment_block {
        finalized_head
    } else {
        deployment_block
    };
    DesktopWalletChainStart {
        start_block: safe_head.saturating_add(1),
        last_scanned_block: safe_head,
    }
}

pub(crate) async fn initialize_new_wallet_chain_metadata_for_session(
    view_session: Arc<vault::DesktopViewSession>,
    effective_chains: settings::EffectiveChainRegistry,
    db: Arc<DbStore>,
    http: HttpContext,
    skip_chain_id: Option<u64>,
    init_policy: CreatedWalletChainInitPolicy,
) -> NewWalletChainMetadataInitReport {
    let vault_store = vault::DesktopVaultStore::from_db(db);
    let mut report = NewWalletChainMetadataInitReport::default();
    let pending_chain_ids = match vault_store.load_wallet_metadata_for_session(&view_session) {
        Ok(metadata) => metadata.pending_create_new_chain_ids,
        Err(error) => {
            tracing::warn!(error = %error, "failed to load pending new-wallet chain metadata initialization");
            report.failed += 1;
            return report;
        }
    };

    for chain_id in pending_chain_ids {
        let Some(effective_chain) = effective_chains.get(chain_id) else {
            report.skipped_unavailable += 1;
            continue;
        };
        if !effective_chain.enabled {
            report.skipped_disabled += 1;
            continue;
        }
        if effective_chain.railgun.is_none() {
            report.skipped_unavailable += 1;
            continue;
        }
        if skip_chain_id == Some(chain_id) {
            report.skipped_selected += 1;
            continue;
        }

        match initialize_new_wallet_chain_metadata_for_chain(
            &vault_store,
            view_session.as_ref(),
            effective_chain,
            &http,
            init_policy,
        )
        .await
        {
            NewWalletChainMetadataInitOutcome::Initialized => {
                report.initialized += 1;
            }
            NewWalletChainMetadataInitOutcome::SkippedExisting => {
                report.skipped_existing += 1;
            }
            NewWalletChainMetadataInitOutcome::Failed => {
                report.failed += 1;
            }
        }
    }

    report
}

async fn initialize_new_wallet_chain_metadata_for_chain(
    vault_store: &vault::DesktopVaultStore,
    view_session: &vault::DesktopViewSession,
    effective_chain: &settings::EffectiveChainConfig,
    http: &HttpContext,
    init_policy: CreatedWalletChainInitPolicy,
) -> NewWalletChainMetadataInitOutcome {
    let chain_id = effective_chain.chain_id;
    let Ok(private) = effective_chain.require_railgun() else {
        return NewWalletChainMetadataInitOutcome::Failed;
    };
    let contract = private.deployment.contract.to_checksum(None);

    match vault_store.find_wallet_chain_metadata_for_session(view_session, 0, chain_id, &contract) {
        Ok(Some(_)) => {
            return complete_new_wallet_chain_metadata_initialization(
                vault_store,
                view_session,
                chain_id,
                &contract,
                NewWalletChainMetadataInitOutcome::SkippedExisting,
            );
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(chain_id, error = %error, "failed to check existing new wallet chain metadata");
            return NewWalletChainMetadataInitOutcome::Failed;
        }
    }

    let baseline = match new_wallet_chain_baseline(init_policy, effective_chain, http).await {
        Ok(baseline) => baseline,
        Err(error) => {
            tracing::warn!(chain_id, error = %error, "retain pending new wallet chain initialization until its baseline is available");
            return NewWalletChainMetadataInitOutcome::Failed;
        }
    };

    match vault_store.find_or_create_wallet_chain_metadata_for_session(
        view_session,
        0,
        chain_id,
        &contract,
        baseline.start_block,
        baseline.last_scanned_block,
    ) {
        Ok((metadata, created)) => {
            let outcome = if created {
                tracing::info!(
                    chain_id,
                    start_block = metadata.start_block,
                    last_scanned_block = metadata.last_scanned_block,
                    "initialized new wallet chain metadata"
                );
                NewWalletChainMetadataInitOutcome::Initialized
            } else {
                NewWalletChainMetadataInitOutcome::SkippedExisting
            };
            complete_new_wallet_chain_metadata_initialization(
                vault_store,
                view_session,
                chain_id,
                &contract,
                outcome,
            )
        }
        Err(error) => {
            tracing::warn!(chain_id, error = %error, "failed to create new wallet chain metadata");
            NewWalletChainMetadataInitOutcome::Failed
        }
    }
}

fn complete_new_wallet_chain_metadata_initialization(
    vault_store: &vault::DesktopVaultStore,
    view_session: &vault::DesktopViewSession,
    chain_id: u64,
    contract: &str,
    outcome: NewWalletChainMetadataInitOutcome,
) -> NewWalletChainMetadataInitOutcome {
    match vault_store.complete_pending_create_new_chain_for_session(
        view_session,
        0,
        chain_id,
        contract,
    ) {
        Ok(_) => outcome,
        Err(error) => {
            tracing::warn!(chain_id, error = %error, "failed to persist new wallet chain initialization completion");
            NewWalletChainMetadataInitOutcome::Failed
        }
    }
}

async fn new_wallet_chain_baseline(
    init_policy: CreatedWalletChainInitPolicy,
    effective_chain: &settings::EffectiveChainConfig,
    http: &HttpContext,
) -> Result<DesktopWalletChainStart> {
    match init_policy {
        CreatedWalletChainInitPolicy::InitialCreate => {
            let head = fetch_effective_chain_head(effective_chain, http).await?;
            Ok(new_wallet_chain_start_from_head(
                effective_chain
                    .require_railgun()?
                    .deployment
                    .deployment_block,
                effective_chain.finality_depth,
                head,
            ))
        }
        CreatedWalletChainInitPolicy::Resumed => Ok(new_wallet_chain_start_from_deployment(
            effective_chain
                .require_railgun()?
                .deployment
                .deployment_block,
        )),
    }
}

async fn fetch_effective_chain_head(
    effective_chain: &settings::EffectiveChainConfig,
    http: &HttpContext,
) -> Result<u64> {
    // A one-shot read verifies every endpoint so the fallback covers all of them.
    let route =
        settings::resolve_effective_chain_rpc_route(effective_chain.chain_id, effective_chain)?
            .verify_identity(&http.rpc_client)
            .await?;
    let rpcs = query_rpc_pool_with_http_client(route.endpoint_urls(), http);
    let providers = rpcs.available_providers();
    if providers.is_empty() {
        return Err(eyre!(
            "no RPC providers configured for chain {}",
            effective_chain.chain_id
        ));
    }

    for provider in providers {
        if let Ok(head) = provider.provider.get_block_number().await {
            return Ok(head);
        }
        tracing::warn!(
            chain_id = effective_chain.chain_id,
            "failed to fetch effective chain head"
        );
        rpcs.mark_bad_provider(&provider);
    }

    Err(eyre!(
        "all RPC providers failed for chain {}",
        effective_chain.chain_id
    ))
}

pub(crate) fn resolve_desktop_wallet_chain_start(
    policy: DesktopWalletSyncStartPolicy,
    existing_metadata: Option<&vault::WalletChainMetadataBundle>,
    init_block_number: Option<u64>,
    deployment_block: u64,
    safe_head: Option<u64>,
    rewind_wallet_cache: bool,
) -> Result<DesktopWalletChainStart> {
    if let Some(metadata) = existing_metadata
        && !rewind_wallet_cache
    {
        return Ok(DesktopWalletChainStart {
            start_block: metadata.start_block,
            last_scanned_block: metadata.last_scanned_block,
        });
    }

    if rewind_wallet_cache {
        let start_block = init_block_number.unwrap_or(deployment_block);
        return Ok(new_wallet_chain_start_from_deployment(start_block));
    }

    match policy {
        DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill => {
            let start_block = init_block_number.unwrap_or(deployment_block);
            Ok(new_wallet_chain_start_from_deployment(start_block))
        }
        DesktopWalletSyncStartPolicy::CurrentSafeHeadNoBackfill => {
            let safe_head = safe_head.ok_or_else(|| {
                eyre!("chain safe head unavailable for generated wallet; retry sync later")
            })?;
            let start_block = safe_head
                .checked_add(1)
                .ok_or_else(|| eyre!("chain safe head overflow for generated wallet"))?;
            Ok(DesktopWalletChainStart {
                start_block,
                last_scanned_block: safe_head,
            })
        }
    }
}

pub(super) async fn setup_synced_view_wallet_with_store(
    view_session: Arc<vault::DesktopViewSession>,
    chain_id: u64,
    sync_start_policy: DesktopWalletSyncStartPolicy,
    init_block_number: Option<u64>,
    sync_to_block: Option<u64>,
    use_indexed_wallet_catch_up: bool,
    effective_chain: settings::EffectiveChainConfig,
    poi_read_source: PoiReadSource,
    rewind_wallet_cache: bool,
    http: &HttpContext,
    progress_tx: Option<SyncProgressSender>,
    wait_until_ready: bool,
    db: Arc<DbStore>,
    sync_manager: Arc<SyncManager>,
) -> Result<SyncedViewWallet> {
    settings::resolve_effective_chain_rpc_route(chain_id, &effective_chain)?;
    let private = effective_chain.require_railgun()?;
    let chain_key = ChainKey {
        chain_id,
        contract: private.deployment.contract,
    };
    let effective_use_indexed_wallet_catch_up =
        use_indexed_wallet_catch_up && private.sync.quick_sync_endpoint.is_some();
    let chain_cfg = verified_chain_config(&effective_chain, http, progress_tx.clone()).await?;
    let wallet_quick_sync_endpoint = chain_cfg.sync.quick_sync_endpoint.clone();
    let chain_service = sync_manager
        .add_chain_with_rpc_http_client(chain_cfg, http.rpc_client.clone())
        .await
        .wrap_err("register chain sync service")?;

    let vault_store = vault::DesktopVaultStore::from_db(Arc::clone(&db));
    let contract = chain_key.contract.to_checksum(None);
    let existing_wallet_chain_metadata = vault_store
        .find_wallet_chain_metadata_for_session(view_session.as_ref(), 0, chain_id, &contract)
        .wrap_err("load encrypted wallet chain metadata")?;
    let chain_handle = chain_service.handle();
    let safe_head = *chain_handle.safe_head_rx.borrow();
    let safe_head = (safe_head > 0).then_some(safe_head);
    let deployment_block = private.deployment.deployment_block;
    let mut resolved_start = resolve_desktop_wallet_chain_start(
        sync_start_policy,
        existing_wallet_chain_metadata.as_ref(),
        init_block_number,
        deployment_block,
        safe_head,
        rewind_wallet_cache,
    )?;
    let mut wallet_chain_metadata = match existing_wallet_chain_metadata {
        Some(metadata) => metadata,
        None => vault_store
            .find_or_create_wallet_chain_metadata_for_session(
                view_session.as_ref(),
                0,
                chain_id,
                &contract,
                resolved_start.start_block,
                resolved_start.last_scanned_block,
            )
            .map(|(metadata, _created)| metadata)
            .wrap_err("find or create encrypted wallet chain metadata")?,
    };
    if !rewind_wallet_cache {
        resolved_start = DesktopWalletChainStart {
            start_block: wallet_chain_metadata.start_block,
            last_scanned_block: wallet_chain_metadata.last_scanned_block,
        };
    }
    tracing::info!(
        chain_id,
        start_block = resolved_start.start_block,
        last_scanned_block = resolved_start.last_scanned_block,
        sync_to_block,
        effective_use_indexed_wallet_catch_up,
        poi_read_source = ?poi_read_source,
        sync_start_policy = ?sync_start_policy,
        "starting desktop view wallet sync"
    );
    vault_store
        .complete_pending_create_new_chain_for_session(
            view_session.as_ref(),
            0,
            chain_id,
            &contract,
        )
        .wrap_err("persist new wallet chain initialization completion")?;
    let start_block = resolved_start.start_block;
    if rewind_wallet_cache {
        wallet_chain_metadata.start_block = start_block;
        vault_store
            .rewind_wallet_chain_cache_with_session(
                view_session.as_ref(),
                &mut wallet_chain_metadata,
                start_block,
            )
            .wrap_err("rewind encrypted wallet cache")?;
        tracing::info!(
            chain_id,
            start_block,
            wallet_chain_uuid = %wallet_chain_metadata.wallet_chain_uuid,
            "rewound encrypted desktop wallet cache"
        );
    }
    let selected_poi_read_source = poi_read_source_label(&poi_read_source);
    if wallet_chain_metadata.poi_read_source.as_deref() != Some(selected_poi_read_source) {
        wallet_chain_metadata.poi_read_source = Some(selected_poi_read_source.to_string());
        vault_store
            .store_wallet_chain_metadata_with_session(view_session.as_ref(), &wallet_chain_metadata)
            .wrap_err("persist selected POI read source")?;
    }
    let cache_key = wallet_chain_metadata
        .wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .wrap_err("parse wallet-chain cache key")?;
    initialize_atomic_wallet_cache_metadata(db.as_ref(), &cache_key, &wallet_chain_metadata)?;
    let cache_store = Arc::new(
        vault::DesktopEncryptedWalletCacheStore::new(
            Arc::clone(&db),
            &view_session,
            wallet_chain_metadata,
        )
        .wrap_err("create encrypted wallet cache")?,
    );
    let scan_keys = view_session.scan_keys();
    let prover_artifact_source = artifact_source(http, db.as_ref())?;
    let poi_recovery_prover = ProverService::new_with_db(&prover_artifact_source, &db);
    let wallet_cfg = WalletConfig {
        chain: chain_key,
        cache_key,
        start_block: Some(start_block),
        sync_to_block,
        quick_sync_endpoint: wallet_quick_sync_endpoint,
        scan_keys,
        spending_public_key: Some(view_session.spending_public_key()),
        progress_tx,
        cache_store: Some(cache_store),
        poi_recovery_prover: Some(poi_recovery_prover),
        use_indexed_wallet_catch_up: effective_use_indexed_wallet_catch_up,
    };

    let mut handle = sync_manager
        .add_wallet(wallet_cfg)
        .await
        .wrap_err("register wallet sync worker")?;
    if wait_until_ready {
        let readiness = handle.wait_until_ready().await;
        finish_waited_wallet_startup(sync_manager.as_ref(), &handle, readiness).await?;
    }

    Ok(SyncedViewWallet {
        db,
        sync_manager,
        chain_key,
        start_block,
        handle,
        public_data_plane: chain_service.public_data_plane(),
    })
}

async fn finish_waited_wallet_startup(
    sync_manager: &SyncManager,
    handle: &WalletHandle,
    readiness: std::result::Result<(), WalletReadinessWaitError>,
) -> Result<()> {
    let Err(error) = readiness else {
        return Ok(());
    };
    let cleanup_error = sync_manager.remove_wallet_session(handle).await.err();
    let context = cleanup_error.map_or_else(
        || "wait for wallet sync worker readiness".to_string(),
        |cleanup_error| {
            format!(
                "wait for wallet sync worker readiness; exact actor cleanup also failed: {cleanup_error}"
            )
        },
    );
    Err::<(), _>(error).wrap_err(context)
}

pub async fn fetch_current_safe_head(
    effective_chain: &settings::EffectiveChainConfig,
    http: &HttpContext,
) -> Result<u64> {
    let private = effective_chain.require_railgun()?;
    let head = fetch_effective_chain_head(effective_chain, http).await?;
    Ok(head
        .saturating_sub(effective_chain.finality_depth)
        .max(private.deployment.deployment_block))
}

async fn verified_chain_config(
    effective_chain: &settings::EffectiveChainConfig,
    http: &HttpContext,
    progress_tx: Option<SyncProgressSender>,
) -> Result<ChainConfig> {
    let mut config = chain_config(effective_chain, http, progress_tx)?;
    let archive = config.archive_rpc_url.clone().map(SensitiveUrl::from);
    // Endpoints join the pool as their identity checks pass; the session starts with the first.
    let mut pool = QueryRpcPool::with_http_client(
        effective_chain.rpc_route.endpoint_urls(),
        DEFAULT_QUERY_RPC_COOLDOWN,
        http.rpc_client.clone(),
    )
    .with_pending_admission();
    if archive.is_some() {
        pool = pool.with_pending_archive();
    }
    let pool = Arc::new(pool);
    effective_chain
        .rpc_route
        .admit_sync_pool(&http.rpc_client, &pool, archive)
        .await?;
    config.rpcs = pool;
    Ok(config)
}

pub(crate) fn chain_config(
    effective_chain: &settings::EffectiveChainConfig,
    http: &HttpContext,
    progress_tx: Option<SyncProgressSender>,
) -> Result<ChainConfig> {
    let private = effective_chain.require_railgun()?;
    let route =
        settings::resolve_effective_chain_rpc_route(effective_chain.chain_id, effective_chain)?;
    let mut sync = private.sync.clone();
    if let Some(source) = &mut sync.indexed_artifact_source {
        source.gateway_pool = Some(http.gateway_pool());
    }
    Ok(ChainConfig {
        deployment: private.deployment,
        sync,
        rpcs: query_rpc_pool_with_http_client(route.endpoint_urls(), http),
        archive_rpc_url: private
            .archive_rpc_url
            .as_ref()
            .map(|url| url.expose_url().clone()),
        block_time: effective_chain
            .block_time
            .ok_or_else(|| eyre!("private sync requires a block cadence"))?,
        finality_depth: effective_chain.finality_depth,
        http_client: http.client.clone(),
        progress_tx,
    })
}

pub(super) const fn poi_read_source_label(poi_read_source: &PoiReadSource) -> &'static str {
    match poi_read_source {
        PoiReadSource::IndexedArtifacts { .. } => "indexed-artifacts",
        PoiReadSource::PoiProxy { .. } => "poi-proxy",
    }
}

pub(super) fn artifact_source(http: &HttpContext, db: &DbStore) -> Result<ArtifactSource> {
    let settings = settings::load_wallet_settings(db).wrap_err("load wallet settings")?;
    let gateways = settings
        .poi
        .artifact
        .gateway_urls
        .iter()
        .map(|gateway| Url::parse(gateway).wrap_err("parse artifact gateway URL"))
        .collect::<Result<Vec<_>>>()?;
    Ok(ArtifactSource::default()
        .with_gateways(gateways)
        .with_gateway_pool(http.gateway_pool())
        .with_client(http.client.clone())
        .with_cache_dir(db.blob_dir().join("artifacts")))
}

pub(super) async fn buffered_gas_price_with_policy(
    provider: &(impl Provider + Clone),
    numerator: u128,
    denominator: u128,
) -> Result<u128> {
    if denominator == 0 {
        return Err(eyre!(
            "gas price buffer denominator must be greater than zero"
        ));
    }
    let gas_price = provider.get_gas_price().await.wrap_err("fetch gas price")?;
    let buffered_gas_price = gas_price * numerator / denominator;
    tracing::debug!(
        target: "gas_price",
        method = "eth_gasPrice",
        rpc_gas_price_wei = gas_price,
        buffer_numerator = numerator,
        buffer_denominator = denominator,
        buffered_gas_price_wei = buffered_gas_price,
        "sampled RPC gas price"
    );
    Ok(buffered_gas_price)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    use super::*;

    static TEMP_DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db_root() -> PathBuf {
        let dir = std::env::temp_dir().join("railoxide-wallet-sync-helper-tests");
        fs::create_dir_all(&dir).expect("create temp db dir");
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let counter = TEMP_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("db-{pid}-{nanos}-{counter}"))
    }

    fn submission_output(commitment: FixedBytes<32>, pending_new: bool) -> UtxoOutput {
        UtxoOutput {
            tree: 0,
            position: u64::from(pending_new),
            token: "0x0000000000000000000000000000000000000001".to_string(),
            value: "1".to_string(),
            commitment_kind: "Transact".to_string(),
            activity_classification: "Private Output".to_string(),
            blocked_shield_rescue: None,
            commitment: hex::encode_prefixed(commitment),
            npk: "0x0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            blinded_commitment:
                "0x0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            poi_statuses: BTreeMap::new(),
            ppoi_state: UtxoPpoiState::Unknown,
            ppoi_last_submission_at: None,
            poi_spendable: false,
            source_tx_hash: "0x0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
            source_block_number: 0,
            source_block_timestamp: 0,
            is_spent: false,
            pending_new,
            pending_spent: false,
            local_pending_spent: false,
            spent_tx_hash: None,
            spent_block_number: None,
        }
    }

    #[test]
    fn ppoi_submission_projection_matches_exact_commitments_and_clears_absent_statuses() {
        let mut outputs = vec![
            submission_output(FixedBytes::from([1; 32]), false),
            submission_output(FixedBytes::from([2; 32]), true),
        ];
        outputs[0].ppoi_last_submission_at = Some(10);
        outputs[1].ppoi_last_submission_at = Some(20);

        apply_ppoi_submission_statuses(
            &mut outputs,
            &[
                WalletPpoiSubmissionStatus {
                    output_commitment: FixedBytes::from([1; 32]),
                    last_submission_at: 100,
                },
                WalletPpoiSubmissionStatus {
                    output_commitment: FixedBytes::from([9; 32]),
                    last_submission_at: 900,
                },
            ],
        );

        assert_eq!(outputs[0].ppoi_last_submission_at, Some(100));
        assert_eq!(outputs[1].ppoi_last_submission_at, None);

        apply_ppoi_submission_statuses(&mut outputs, &[]);
        assert!(
            outputs
                .iter()
                .all(|output| output.ppoi_last_submission_at.is_none())
        );
    }

    #[test]
    fn artifact_source_uses_db_blob_artifacts_dir() {
        let root_dir = temp_db_root();
        let db = DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open test db");
        let mut settings = settings::WalletSettings::default();
        settings.poi.artifact.gateway_urls = vec!["https://gateway.example".to_string()];
        settings::save_wallet_settings(&db, &settings).expect("save wallet settings");

        let http = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(build_wallet_network_context(WalletNetworkConfig {
                network_mode: Some(WalletNetworkMode::Direct),
                proxy: None,
                data_dir: &root_dir,
            }))
            .expect("http context");
        let source = artifact_source(&http, &db).expect("artifact source");

        assert_eq!(source.out_dir, db.blob_dir().join("artifacts"));
        assert_eq!(source.gateways[0].as_str(), "https://gateway.example/");
        assert!(source.client.is_some());
        fs::remove_dir_all(root_dir).expect("remove temp db dir");
    }

    /// Run with the four verified `<CID>.car` files in `PPOI_ARTIFACT_CARS`:
    /// `PPOI_ARTIFACT_CARS=/tmp/ppoi-desktop-cars cargo test --locked -p wallet-ops --lib desktop::sync_helpers::tests::desktop_ppoi_artifact_upgrade_preserves_existing_db -- --ignored --exact --nocapture`
    #[tokio::test]
    #[ignore = "requires provisioned PPOI artifact CARs"]
    async fn desktop_ppoi_artifact_upgrade_preserves_existing_db() {
        use sha2::{Digest, Sha256};
        use std::sync::atomic::AtomicBool;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const BUNDLE: &str = "QmZ2MyM6TKxffkv6stuo2hFwmUfs3q4xgMYN164Sje8new";
        const CIDS: [&str; 4] = [
            "QmYG6a72rcLiddX1DyHmDvRaDpkPAf3VdHNNPxVBqndhEy",
            "QmaNNJxu5DqzmB3W6BSR49bsPM1MNhwWH9o4Dqd8cSpeyL",
            "QmTY2yzKjMhjdcgC52Q3aiHE5wNRUsWgdDtzFtqSFfuzXm",
            "QmUtQFFYcmooihjn5wfYdVNPbtkEv4SKqegwcdcd1L3K5X",
        ];
        const HASHES: [(usize, &str, &str); 2] = [
            (
                3,
                "a128e273f8a7b9fa9e04e17da079b89a57e416db845864d0d5c88570564a2066",
                "b82a6d545d94cb774592b652b3d6b3d73f032eac946119b5de631d0609da7cbe",
            ),
            (
                13,
                "1ec7c1230a3985f752c5cbeafedcb152902d0d293d9b69477032a0d7d736ef53",
                "49a0c7b654d8d70157164a16702d9de15e9d2176803a999c631fa6e27a6a5a81",
            ),
        ];

        struct AbortServerOnDrop(tokio::task::JoinHandle<()>);

        impl Drop for AbortServerOnDrop {
            fn drop(&mut self) {
                self.0.abort();
            }
        }

        let car_dir =
            PathBuf::from(std::env::var_os("PPOI_ARTIFACT_CARS").expect("set PPOI_ARTIFACT_CARS"));
        let cars = CIDS
            .into_iter()
            .map(|cid| {
                (
                    cid.to_string(),
                    fs::read(car_dir.join(format!("{cid}.car"))).expect("read provisioned CAR"),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local artifact gateway");
        let gateway = format!(
            "http://{}",
            listener.local_addr().expect("local gateway address")
        );
        let requests = Arc::new(AtomicU64::new(0));
        let unavailable = Arc::new(AtomicBool::new(false));
        let server_requests = Arc::clone(&requests);
        let server_unavailable = Arc::clone(&unavailable);
        let mut server = AbortServerOnDrop(tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.expect("accept artifact request");
                tokio::time::timeout(Duration::from_secs(10), async {
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        assert!(request.len() < 8192, "artifact request header too large");
                        request.push(socket.read_u8().await.expect("read artifact request"));
                    }
                    let request = String::from_utf8(request).expect("HTTP request text");
                    let target = request
                        .split_whitespace()
                        .nth(1)
                        .expect("artifact request target");
                    assert!(target.contains("format=car"), "request must use a CAR");
                    let cid = target
                        .strip_prefix("/ipfs/")
                        .expect("IPFS artifact path")
                        .split('?')
                        .next()
                        .expect("artifact CID");
                    let car = cars.get(cid).expect("request must use a current artifact CID");
                    server_requests.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = if server_unavailable.load(Ordering::SeqCst) {
                        ("503 Service Unavailable", &[][..])
                    } else {
                        ("200 OK", car.as_slice())
                    };
                    let header = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/vnd.ipld.car\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(header.as_bytes()).await.expect("write CAR header");
                    socket.write_all(body).await.expect("write CAR body");
                })
                .await
                .expect("artifact server request deadline");
            }
        }));

        for upgraded in [false, true] {
            let root_dir = temp_db_root();
            let db = DbStore::open(DbConfig {
                root_dir: root_dir.clone(),
            })
            .expect("open test db");
            let mut saved_settings = settings::WalletSettings::default();
            saved_settings.poi.artifact.gateway_urls = vec![gateway.clone()];
            settings::save_wallet_settings(&db, &saved_settings).expect("save artifact gateway");
            let cache_key = WalletCacheKey::from_opaque_id([0x42; 16]);
            let checkpoint = vault::WalletChainMetadataBundle {
                wallet_chain_uuid: cache_key.to_string(),
                wallet_uuid: "synthetic-artifact-upgrade".to_string(),
                chain_type: 0,
                chain_id: 1,
                contract: "0x1111111111111111111111111111111111111111".to_string(),
                start_block: 100,
                last_scanned_block: 149,
                last_scanned_block_hash: Some([0x33; 32]),
                poi_read_source: None,
            };
            initialize_atomic_wallet_cache_metadata(&db, &cache_key, &checkpoint)
                .expect("save checkpoint sentinel");
            let artifact_dir = db.blob_dir().join("artifacts");
            let mut sentinels = Vec::new();
            if upgraded {
                for variant in [
                    "01x01",
                    "artifacts-v2.1/poi-nov-2-23/POI_3x3",
                    "artifacts-v2.1/poi-nov-2-23/POI_13x13",
                ] {
                    for file in ["zkey", "wasm"] {
                        let path = artifact_dir.join(variant).join(file);
                        fs::create_dir_all(path.parent().expect("sentinel directory"))
                            .expect("create old artifact directory");
                        let bytes = format!("retained {variant}/{file}").into_bytes();
                        fs::write(&path, &bytes).expect("seed artifact sentinel");
                        sentinels.push((path, bytes));
                    }
                }
            }
            let http = build_wallet_network_context(WalletNetworkConfig {
                network_mode: Some(WalletNetworkMode::Direct),
                proxy: None,
                data_dir: &root_dir,
            })
            .await
            .expect("shared direct HTTP context");
            let before = requests.load(Ordering::SeqCst);
            let source = artifact_source(&http, &db).expect("desktop artifact source");
            for (size, zkey_hash, wasm_hash) in HASHES {
                let paths = tokio::time::timeout(
                    Duration::from_secs(60),
                    source.ensure_poi_artifacts(size, size),
                )
                .await
                .expect("artifact acquisition deadline")
                .expect("acquire current artifacts through desktop factory");
                let expected_dir =
                    artifact_dir.join(format!("artifacts-v2.1/{BUNDLE}/POI_{size}x{size}"));
                assert_eq!(paths.zkey, expected_dir.join("zkey"));
                assert_eq!(paths.wasm, expected_dir.join("wasm"));
                for (path, expected_hash) in [(paths.zkey, zkey_hash), (paths.wasm, wasm_hash)] {
                    assert_eq!(
                        hex::encode(Sha256::digest(
                            fs::read(path).expect("read current artifact")
                        )),
                        expected_hash
                    );
                }
            }
            assert_eq!(requests.load(Ordering::SeqCst), before + 4);
            drop(source);
            drop(db);

            let db = DbStore::open(DbConfig {
                root_dir: root_dir.clone(),
            })
            .expect("reopen test db");
            let source = artifact_source(&http, &db).expect("recreate desktop artifact source");
            for (size, _, _) in HASHES {
                source
                    .ensure_poi_artifacts(size, size)
                    .await
                    .expect("reuse current artifacts after restart");
            }
            assert_eq!(
                requests.load(Ordering::SeqCst),
                before + 4,
                "restart must reuse downloaded artifacts"
            );
            assert_eq!(
                settings::load_wallet_settings(&db).expect("load saved settings"),
                saved_settings
            );
            let stored = db
                .get_wallet_meta(&cache_key)
                .expect("load checkpoint")
                .expect("checkpoint retained");
            assert_eq!(stored.last_scanned_block, checkpoint.last_scanned_block);
            assert_eq!(
                stored.last_scanned_block_hash,
                checkpoint.last_scanned_block_hash
            );
            for (path, bytes) in sentinels {
                assert_eq!(fs::read(path).expect("retained artifact sentinel"), bytes);
            }
            drop(source);
            drop(db);
            drop(http);
            fs::remove_dir_all(root_dir).expect("remove test db");
        }

        unavailable.store(true, Ordering::SeqCst);
        let root_dir = temp_db_root();
        let db = DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open legacy-only db");
        let mut saved_settings = settings::WalletSettings::default();
        saved_settings.poi.artifact.gateway_urls = vec![gateway];
        settings::save_wallet_settings(&db, &saved_settings).expect("save unavailable gateway");
        let legacy = db
            .blob_dir()
            .join("artifacts/artifacts-v2.1/poi-nov-2-23/POI_3x3");
        fs::create_dir_all(&legacy).expect("create legacy-only directory");
        fs::write(legacy.join("zkey"), b"legacy zkey").expect("seed legacy zkey");
        fs::write(legacy.join("wasm"), b"legacy wasm").expect("seed legacy wasm");
        let http = build_wallet_network_context(WalletNetworkConfig {
            network_mode: Some(WalletNetworkMode::Direct),
            proxy: None,
            data_dir: &root_dir,
        })
        .await
        .expect("shared HTTP context for legacy-only db");
        let source = artifact_source(&http, &db).expect("legacy-only desktop artifact source");
        let error =
            tokio::time::timeout(Duration::from_secs(10), source.ensure_poi_artifacts(3, 3))
                .await
                .expect("unavailable acquisition deadline")
                .expect_err("legacy artifacts must not satisfy a current artifact request");
        assert!(matches!(
            &error,
            railgun_wallet::artifacts::ArtifactError::Trustless(_)
        ));
        assert!(
            error.to_string().contains("503"),
            "acquisition error must identify the HTTP failure: {error}"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 9);
        let current = source.artifact_paths("POI_3x3");
        assert!(!current.zkey.exists());
        assert!(!current.wasm.exists());
        assert_eq!(
            fs::read(legacy.join("zkey")).expect("retained legacy zkey"),
            b"legacy zkey"
        );
        assert_eq!(
            fs::read(legacy.join("wasm")).expect("retained legacy wasm"),
            b"legacy wasm"
        );
        drop(source);
        drop(db);
        drop(http);
        fs::remove_dir_all(root_dir).expect("remove legacy-only db");
        server.0.abort();
        let stopped = tokio::time::timeout(Duration::from_secs(5), &mut server.0)
            .await
            .expect("artifact server shutdown deadline")
            .expect_err("artifact server should stop on abort");
        assert!(stopped.is_cancelled());
    }

    #[test]
    fn alpha_wallet_cache_metadata_initializes_once_from_chain_metadata() {
        let root_dir = temp_db_root();
        let db = DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open test db");
        let cache_key = WalletCacheKey::from_opaque_id([0x42; 16]);
        let mut metadata = vault::WalletChainMetadataBundle {
            wallet_chain_uuid: cache_key.to_string(),
            wallet_uuid: "wallet".to_string(),
            chain_type: 0,
            chain_id: 1,
            contract: "0x1111111111111111111111111111111111111111".to_string(),
            start_block: 100,
            last_scanned_block: 149,
            last_scanned_block_hash: Some([0x33; 32]),
            poi_read_source: None,
        };

        initialize_atomic_wallet_cache_metadata(&db, &cache_key, &metadata)
            .expect("initialize atomic metadata");
        metadata.last_scanned_block = 999;
        initialize_atomic_wallet_cache_metadata(&db, &cache_key, &metadata)
            .expect("repeat atomic metadata initialization");

        let stored = db
            .get_wallet_meta(&cache_key)
            .expect("load atomic metadata")
            .expect("atomic metadata present");
        assert_eq!(stored.last_scanned_block, 149);
        assert_eq!(stored.last_scanned_block_hash, Some([0x33; 32]));

        drop(db);
        fs::remove_dir_all(root_dir).expect("remove temp db dir");
    }

    #[test]
    fn waited_startup_failure_removes_only_the_failed_actor() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime")
            .block_on(async {
                let root_dir = temp_db_root();
                let db = Arc::new(
                    DbStore::open(DbConfig {
                        root_dir: root_dir.clone(),
                    })
                    .expect("open test db"),
                );
                let rpc_url = Url::parse("http://127.0.0.1:1").expect("test RPC URL");
                let chain_key = ChainKey {
                    chain_id: 1,
                    contract: Address::ZERO,
                };
                let sync_manager = SyncManager::new(
                    Arc::clone(&db),
                    PoiReadSource::PoiProxy {
                        rpc_url: rpc_url.clone().into(),
                    },
                )
                .expect("acquire test sync manager ownership");
                sync_manager
                    .add_chain(ChainConfig {
                        deployment: broadcaster_core::deployment::RailgunDeployment {
                            chain_id: chain_key.chain_id,
                            contract: chain_key.contract,
                            relay_adapt_contract: Address::ZERO,
                            relay_adapt_history: &[],
                            relay_adapt_7702_contract: Address::ZERO,
                            deployment_block: 0,
                            v2_start_block: 0,
                            legacy_shield_block: 0,
                        },
                        sync: sync_service::RailgunSyncOptions {
                            archive_until_block: 0,
                            block_range: 100,
                            indexed_wallet_block_range: 100,
                            poll_interval: Duration::from_mins(1),
                            quick_sync_endpoint: None,
                            indexed_artifact_source: None,
                            anchor_interval: 1000,
                            anchor_retention: 5,
                        },
                        rpcs: Arc::new(QueryRpcPool::new(
                            vec![rpc_url.clone()],
                            Duration::from_millis(1),
                        )),
                        archive_rpc_url: None,
                        block_time: Duration::from_secs(12),
                        finality_depth: 0,
                        http_client: reqwest::Client::new(),
                        progress_tx: None,
                    })
                    .await
                    .expect("add test chain");
                let cache_key = WalletCacheKey::from_opaque_bytes(b"waited-startup-cleanup")
                    .expect("test cache key");
                let wallet_cfg = WalletConfig {
                    chain: chain_key,
                    cache_key: cache_key.clone(),
                    start_block: Some(0),
                    sync_to_block: Some(0),
                    quick_sync_endpoint: None,
                    scan_keys: broadcaster_core::crypto::railgun::ViewingKeyData {
                        viewing_private_key: [0; 32],
                        viewing_public_key: [0; 32],
                        nullifying_key: U256::ZERO,
                        master_public_key: U256::ZERO,
                    },
                    spending_public_key: None,
                    progress_tx: None,
                    cache_store: None,
                    poi_recovery_prover: None,
                    use_indexed_wallet_catch_up: false,
                };
                let failed = sync_manager
                    .add_wallet(wallet_cfg.clone())
                    .await
                    .expect("register failed actor fixture");
                sync_manager
                    .remove_wallet_session(&failed)
                    .await
                    .expect("retire failed actor fixture");
                let replacement = sync_manager
                    .add_wallet(wallet_cfg)
                    .await
                    .expect("register replacement actor");

                let error = finish_waited_wallet_startup(
                    &sync_manager,
                    &failed,
                    Err(WalletReadinessWaitError::Failed(
                        WalletReadinessError::ApplyFailed,
                    )),
                )
                .await
                .expect_err("startup failure is propagated");
                assert_eq!(
                    error.downcast_ref::<WalletReadinessWaitError>(),
                    Some(&WalletReadinessWaitError::Failed(
                        WalletReadinessError::ApplyFailed
                    ))
                );
                assert!(
                    sync_manager
                        .wallet_handle(&chain_key, cache_key.as_str())
                        .await
                        .is_some(),
                    "exact cleanup must not remove a replacement actor",
                );

                sync_manager
                    .remove_wallet_session(&replacement)
                    .await
                    .expect("remove replacement actor");
                sync_manager.shutdown().await;
                drop(sync_manager);
                drop(db);
                fs::remove_dir_all(root_dir).expect("remove temp db dir");
            });
    }
}
