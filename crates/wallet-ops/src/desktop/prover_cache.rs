use super::{
    ACTIVE_PROVER_CACHE_BUILDS, Arc, DbConfig, DbStore, HttpContext, Path, PathBuf,
    ProverCacheBuildProgress, ProverCacheBuildReport, Result, Url, WalletNetworkConfig,
    WalletNetworkMode, WasmModuleCacheKey, WrapErr, artifact_source,
    build_prover_cache_with_progress, build_wallet_network_context, watch,
};
use eyre::eyre;

#[derive(Debug, Clone)]
pub struct BuildCacheRequest {
    pub db_path: PathBuf,
    pub network_mode: Option<WalletNetworkMode>,
    pub proxy: Option<Url>,
    /// Without a key the build skips native circuit module compilation.
    pub wasm_module_cache_key: Option<Arc<WasmModuleCacheKey>>,
}

pub struct ProverCacheBuildSession {
    db_path: PathBuf,
    progress_tx: watch::Sender<Option<ProverCacheBuildProgress>>,
}

impl Drop for ProverCacheBuildSession {
    fn drop(&mut self) {
        let _ = self.progress_tx.send(None);
        let mut active = ACTIVE_PROVER_CACHE_BUILDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.remove(&self.db_path);
    }
}

impl ProverCacheBuildSession {
    fn publish(&self, progress: ProverCacheBuildProgress) {
        let _ = self.progress_tx.send(Some(progress));
    }
}

fn prover_cache_build_key(db_path: &Path) -> PathBuf {
    db_path
        .canonicalize()
        .unwrap_or_else(|_| db_path.to_path_buf())
}

pub fn begin_prover_cache_build(db_path: &Path) -> Result<ProverCacheBuildSession> {
    let db_path = prover_cache_build_key(db_path);
    let (progress_tx, _) = watch::channel(Some(ProverCacheBuildProgress::preparing()));
    {
        let mut active = ACTIVE_PROVER_CACHE_BUILDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.contains_key(&db_path) {
            return Err(eyre!(
                "prover cache build is already running for {}",
                db_path.display()
            ));
        }
        active.insert(db_path.clone(), progress_tx.clone());
    }
    Ok(ProverCacheBuildSession {
        db_path,
        progress_tx,
    })
}

pub fn subscribe_prover_cache_build(
    db_path: &Path,
) -> Option<watch::Receiver<Option<ProverCacheBuildProgress>>> {
    let db_path = prover_cache_build_key(db_path);
    let active = ACTIVE_PROVER_CACHE_BUILDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    active.get(&db_path).map(watch::Sender::subscribe)
}

pub async fn build_cache(request: BuildCacheRequest) -> Result<ProverCacheBuildReport> {
    let session = begin_prover_cache_build(&request.db_path)?;
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: request.db_path.clone(),
        })
        .wrap_err("open local db")?,
    );
    let http = build_wallet_network_context(WalletNetworkConfig {
        network_mode: request.network_mode,
        proxy: request.proxy.as_ref(),
        data_dir: &request.db_path,
    })
    .await?;
    build_cache_with_context_and_progress_with_session(
        db,
        &http,
        request.wasm_module_cache_key,
        session,
        |_| {},
    )
    .await
}

pub async fn build_cache_with_context(
    db: Arc<DbStore>,
    http: &HttpContext,
    wasm_module_cache_key: Option<Arc<WasmModuleCacheKey>>,
) -> Result<ProverCacheBuildReport> {
    build_cache_with_context_and_progress(db, http, wasm_module_cache_key, |_| {}).await
}

pub async fn build_cache_with_context_and_progress(
    db: Arc<DbStore>,
    http: &HttpContext,
    wasm_module_cache_key: Option<Arc<WasmModuleCacheKey>>,
    mut on_progress: impl FnMut(ProverCacheBuildProgress) + Send + 'static,
) -> Result<ProverCacheBuildReport> {
    let session = begin_prover_cache_build(db.root_dir())?;
    build_cache_with_context_and_progress_with_session(
        db,
        http,
        wasm_module_cache_key,
        session,
        move |progress| {
            on_progress(progress);
        },
    )
    .await
}

/// The key is fixed for the whole build. Without it, artifacts are downloaded and proving
/// keys cached, but native circuit modules are left to compile on first use.
pub async fn build_cache_with_context_and_progress_with_session(
    db: Arc<DbStore>,
    http: &HttpContext,
    wasm_module_cache_key: Option<Arc<WasmModuleCacheKey>>,
    session: ProverCacheBuildSession,
    mut on_progress: impl FnMut(ProverCacheBuildProgress) + Send + 'static,
) -> Result<ProverCacheBuildReport> {
    let source = artifact_source(http, db.as_ref())?;
    let db_path = db.root_dir().to_path_buf();
    tracing::info!(
        db_path = %db_path.display(),
        network_mode = %http.network_mode(),
        artifact_dir = %source.out_dir.display(),
        "starting wallet cache build"
    );
    let report = build_prover_cache_with_progress(
        &source,
        Some(Arc::clone(&db)),
        wasm_module_cache_key,
        |progress| {
            session.publish(progress.clone());
            on_progress(progress);
        },
    )
    .await?;
    tracing::info!(
        railgun_variants = report.railgun_variants,
        poi_variants = report.poi_variants,
        total_variants = report.total_variants,
        succeeded_variants = report.succeeded_variants,
        failed_variants = report.failed_variants,
        elapsed_ms = report.elapsed_ms,
        native_module_cache_enabled = report.native_module_cache_enabled,
        "wallet cache build complete"
    );
    Ok(report)
}
