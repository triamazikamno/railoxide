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
    effective_chains: BTreeMap<u64, settings::EffectiveChainConfig>,
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
        let Some(effective_chain) = effective_chains.get(&chain_id) else {
            report.skipped_unavailable += 1;
            continue;
        };
        if !effective_chain.enabled {
            report.skipped_disabled += 1;
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
    let chain_defaults = match chain_defaults_for_chain(chain_id) {
        Ok(defaults) => defaults,
        Err(error) => {
            tracing::warn!(chain_id, error = %error, "skip new wallet chain metadata for unsupported chain");
            return NewWalletChainMetadataInitOutcome::Failed;
        }
    };
    let contract = match parse_effective_address(
        "railgun contract",
        &effective_chain.railgun_contract,
    ) {
        Ok(contract) => contract.to_checksum(None),
        Err(error) => {
            tracing::warn!(chain_id, error = %error, "skip new wallet chain metadata for invalid contract");
            return NewWalletChainMetadataInitOutcome::Failed;
        }
    };

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

    let baseline = match new_wallet_chain_baseline(
        init_policy,
        &chain_defaults,
        effective_chain,
        http,
    )
    .await
    {
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
    defaults: &ChainConfigDefaults,
    effective_chain: &settings::EffectiveChainConfig,
    http: &HttpContext,
) -> Result<DesktopWalletChainStart> {
    match init_policy {
        CreatedWalletChainInitPolicy::InitialCreate => {
            let head = fetch_effective_chain_head(defaults, effective_chain, http).await?;
            Ok(new_wallet_chain_start_from_head(
                effective_chain.deployment_block,
                effective_chain.finality_depth,
                head,
            ))
        }
        CreatedWalletChainInitPolicy::Resumed => Ok(new_wallet_chain_start_from_deployment(
            effective_chain.deployment_block,
        )),
    }
}

async fn fetch_effective_chain_head(
    defaults: &ChainConfigDefaults,
    effective_chain: &settings::EffectiveChainConfig,
    http: &HttpContext,
) -> Result<u64> {
    let chain_cfg = chain_config(defaults, None, Some(effective_chain), http, None)?;
    let providers = chain_cfg.rpcs.available_providers();
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
        chain_cfg.rpcs.mark_bad_provider(&provider);
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
    effective_chain: Option<settings::EffectiveChainConfig>,
    poi_read_source: PoiReadSource,
    rewind_wallet_cache: bool,
    rpc_url_override: Option<Url>,
    http: &HttpContext,
    progress_tx: Option<SyncProgressSender>,
    wait_until_ready: bool,
    db: Arc<DbStore>,
    sync_manager: Arc<SyncManager>,
) -> Result<SyncedViewWallet> {
    let chain_defaults = chain_defaults_for_chain(chain_id)?;
    let effective_contract = effective_chain
        .as_ref()
        .map(|chain| parse_effective_address("railgun contract", &chain.railgun_contract))
        .transpose()?;
    let chain_key = ChainKey {
        chain_id: chain_defaults.chain_id,
        contract: effective_contract.unwrap_or(chain_defaults.contract),
    };

    let effective_use_indexed_wallet_catch_up = effective_chain
        .as_ref()
        .map_or(use_indexed_wallet_catch_up, |chain| {
            use_indexed_wallet_catch_up && chain.quick_sync_enabled
        });
    let chain_cfg = chain_config(
        &chain_defaults,
        rpc_url_override,
        effective_chain.as_ref(),
        http,
        progress_tx.clone(),
    )?;
    let wallet_quick_sync_endpoint = chain_cfg.quick_sync_endpoint.clone();
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
    let deployment_block = effective_chain
        .as_ref()
        .map_or(chain_defaults.deployment_block, |chain| {
            chain.deployment_block
        });
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

pub(crate) fn chain_defaults_for_chain(chain_id: u64) -> Result<ChainConfigDefaults> {
    ChainConfigDefaults::for_chain(chain_id).ok_or_else(|| eyre!("unsupported chain id {chain_id}"))
}

pub async fn fetch_current_safe_head(
    effective_chain: &settings::EffectiveChainConfig,
    http: &HttpContext,
) -> Result<u64> {
    let defaults = chain_defaults_for_chain(effective_chain.chain_id)?;
    let head = fetch_effective_chain_head(&defaults, effective_chain, http).await?;
    Ok(head
        .saturating_sub(effective_chain.finality_depth)
        .max(effective_chain.deployment_block))
}

pub(crate) fn chain_config(
    defaults: &ChainConfigDefaults,
    rpc_url_override: Option<Url>,
    effective_chain: Option<&settings::EffectiveChainConfig>,
    http: &HttpContext,
    progress_tx: Option<SyncProgressSender>,
) -> Result<ChainConfig> {
    let rpc_urls = if effective_chain.is_some() {
        effective_rpc_urls_for_chain(defaults, effective_chain)?
    } else if let Some(rpc_url) = rpc_url_override {
        vec![rpc_url]
    } else {
        defaults.rpc_urls.clone()
    };
    let quick_sync_endpoint = effective_chain
        .filter(|chain| chain.quick_sync_enabled)
        .and_then(|chain| chain.quick_sync_endpoint.as_ref())
        .map(|url| Url::parse(url).wrap_err_with(|| format!("parse quick-sync URL {url}")))
        .transpose()?
        .or_else(|| {
            effective_chain
                .is_none()
                .then(|| defaults.quick_sync_endpoint.clone())
                .flatten()
        });
    let contract = effective_chain
        .map(|chain| parse_effective_address("railgun contract", &chain.railgun_contract))
        .transpose()?
        .unwrap_or(defaults.contract);
    let archive_rpc_url = effective_chain
        .and_then(|chain| chain.archive_rpc_url.as_ref())
        .map(|url| Url::parse(url).wrap_err_with(|| format!("parse archive RPC URL {url}")))
        .transpose()?;
    let query_rpc_pool = Arc::new(QueryRpcPool::with_http_client(
        rpc_urls,
        DEFAULT_QUERY_RPC_COOLDOWN,
        http.rpc_client.clone(),
    ));

    Ok(ChainConfig {
        chain_id: defaults.chain_id,
        contract,
        rpcs: query_rpc_pool,
        archive_rpc_url,
        archive_until_block: effective_chain.map_or(defaults.archive_until_block, |chain| {
            chain.archive_until_block
        }),
        deployment_block: effective_chain
            .map_or(defaults.deployment_block, |chain| chain.deployment_block),
        v2_start_block: effective_chain
            .map_or(defaults.v2_start_block, |chain| chain.v2_start_block),
        legacy_shield_block: effective_chain.map_or(defaults.legacy_shield_block, |chain| {
            chain.legacy_shield_block
        }),
        block_range: effective_chain
            .and_then(|chain| chain.block_range)
            .unwrap_or(DEFAULT_BLOCK_RANGE),
        indexed_wallet_block_range: effective_chain
            .map_or(defaults.indexed_wallet_block_range, |chain| {
                chain.indexed_wallet_block_range
            }),
        block_time: effective_chain.map_or(defaults.block_time, |chain| chain.block_time),
        poll_interval: effective_chain
            .and_then(|chain| chain.poll_interval_secs)
            .map_or(DEFAULT_POLL_INTERVAL, Duration::from_secs),
        finality_depth: effective_chain
            .map_or(defaults.finality_depth, |chain| chain.finality_depth),
        quick_sync_endpoint,
        indexed_artifact_source: effective_chain
            .and_then(|chain| chain.indexed_artifact_source.as_ref())
            .map(|source| sync_service::IndexedArtifactSourceConfig {
                trusted_publisher_pubkey: source.trusted_publisher_pubkey,
                manifest_source: match &source.manifest_source {
                    settings::IndexedArtifactManifestSource::Url(url) => {
                        sync_service::IndexedArtifactManifestSource::Url(url.clone())
                    }
                    settings::IndexedArtifactManifestSource::Cid(cid) => {
                        sync_service::IndexedArtifactManifestSource::Cid(cid.clone())
                    }
                    settings::IndexedArtifactManifestSource::IpnsName(name) => {
                        sync_service::IndexedArtifactManifestSource::IpnsName(name.clone())
                    }
                },
                gateway_urls: source.gateway_urls.clone(),
                gateway_pool: Some(http.gateway_pool()),
                max_manifest_age: source.max_manifest_age,
                concurrency: source.concurrency,
                max_in_flight_bytes: source.max_in_flight_bytes,
            }),
        anchor_interval: defaults.anchor_interval,
        anchor_retention: defaults.anchor_retention,
        http_client: Some(http.client.clone()),
        progress_tx,
    })
}

pub(super) fn parse_effective_address(label: &str, value: &str) -> Result<Address> {
    Address::from_str(value).wrap_err_with(|| format!("parse effective {label} address"))
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
    Ok(gas_price * numerator / denominator)
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
                        chain_id: chain_key.chain_id,
                        contract: chain_key.contract,
                        rpcs: Arc::new(QueryRpcPool::new(
                            vec![rpc_url.clone()],
                            Duration::from_millis(1),
                        )),
                        archive_rpc_url: None,
                        archive_until_block: 0,
                        deployment_block: 0,
                        v2_start_block: 0,
                        legacy_shield_block: 0,
                        block_range: 100,
                        indexed_wallet_block_range: 100,
                        block_time: Duration::from_secs(12),
                        poll_interval: Duration::from_mins(1),
                        finality_depth: 0,
                        quick_sync_endpoint: None,
                        indexed_artifact_source: None,
                        anchor_interval: 1000,
                        anchor_retention: 5,
                        http_client: None,
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
