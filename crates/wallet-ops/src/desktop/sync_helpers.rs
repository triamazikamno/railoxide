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
            .verify_identity(&http.rpc_clients())
            .await?
            .for_class(RpcTrafficClass::General)?;
    let rpcs = query_rpc_pool_for_route(&route, http);
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
    let effective_use_indexed_wallet_catch_up = use_indexed_wallet_catch_up
        && (private.sync.quick_sync_endpoint.is_some()
            || private.sync.indexed_artifact_source.is_some());
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
    let private = effective_chain.require_railgun()?;
    let archive = private
        .archive_rpc_url
        .clone()
        .map(|url| (url, private.archive_rpc_transport));
    let clients = http.rpc_clients();
    let route = effective_chain
        .rpc_route
        .for_class(RpcTrafficClass::General)?;
    // Endpoints join the pool as their identity checks pass; the session starts with the first.
    let mut pool = route
        .query_rpc_pool(&clients, DEFAULT_QUERY_RPC_COOLDOWN)
        .with_pending_admission();
    if archive.is_some() {
        pool = pool.with_pending_archive();
    }
    let pool = Arc::new(pool);
    route.admit_sync_pool(&clients, &pool, archive).await?;
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
        settings::resolve_effective_chain_rpc_route(effective_chain.chain_id, effective_chain)?
            .for_class(RpcTrafficClass::General)?;
    let mut sync = private.sync.clone();
    if let Some(source) = &mut sync.indexed_artifact_source {
        source.gateway_pool = Some(http.gateway_pool());
    }
    Ok(ChainConfig {
        deployment: private.deployment,
        sync,
        rpcs: query_rpc_pool_for_route(&route, http),
        archive_rpc_url: private
            .archive_rpc_url
            .as_ref()
            .map(|url| url.expose_url().clone()),
        archive_rpc_http_client: (private.archive_rpc_transport == RpcTransport::Direct)
            .then(|| http.direct_rpc_client.clone()),
        block_time: effective_chain
            .block_time
            .ok_or_else(|| eyre!("private sync requires a block cadence"))?,
        finality_depth: effective_chain.finality_depth,
        http_client: http.client.clone(),
        progress_tx,
        // A network mode change restarts sync, so the choice holds for this config's life.
        poi_submission_routes: (http.network_mode() == WalletNetworkMode::Tor).then(|| {
            Arc::new(OperationPoiSubmissionRoutes { http: http.clone() })
                as Arc<dyn sync_service::PoiSubmissionRouteProvider>
        }),
    })
}

/// Opens one built-in Tor route per Railgun transaction's PPOI submissions.
struct OperationPoiSubmissionRoutes {
    http: HttpContext,
}

#[async_trait::async_trait]
impl sync_service::PoiSubmissionRouteProvider for OperationPoiSubmissionRoutes {
    async fn isolated_route(
        &self,
    ) -> std::result::Result<sync_service::PoiSubmissionRoute, sync_service::PoiSubmissionRouteError>
    {
        let operation = self.http.operation_http_client().await.map_err(|error| {
            sync_service::PoiSubmissionRouteError::new(format!(
                "open PPOI submission route: {error}"
            ))
        })?;
        if operation.isolation() != OperationNetworkIsolation::Dedicated {
            return Err(sync_service::PoiSubmissionRouteError::new(
                "PPOI submission route is not isolated",
            ));
        }
        // The route keeps `operation`, and with it the loopback bridge, alive.
        Ok(sync_service::PoiSubmissionRoute::new(
            operation.client().clone(),
            operation,
        ))
    }
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
    let gas_price = provider.get_gas_price().await.wrap_err("fetch gas price")?;
    buffer_gas_price(gas_price, numerator, denominator)
}

pub(super) fn buffer_gas_price(
    gas_price: u128,
    numerator: u128,
    denominator: u128,
) -> Result<u128> {
    if denominator == 0 {
        return Err(eyre!(
            "gas price buffer denominator must be greater than zero"
        ));
    }
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
mod tests;
