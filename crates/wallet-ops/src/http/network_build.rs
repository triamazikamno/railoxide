#[allow(clippy::wildcard_imports)]
use super::*;

/// Compatibility constructor for non-wallet call sites. Wallet binaries should
/// use [`build_wallet_network_context`] so the default is built-in Tor.
pub fn build_http_client(proxy: Option<&Url>) -> Result<HttpContext> {
    let mode = if proxy.is_some() {
        WalletNetworkMode::Proxy
    } else {
        WalletNetworkMode::Direct
    };
    build_reqwest_context(mode, proxy.cloned(), proxy.cloned(), None, None, None, None)
}

pub fn request_tor_state_reset(data_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(data_dir)
        .wrap_err_with(|| format!("create wallet data directory {}", data_dir.display()))?;
    let marker_path = tor_state_reset_marker_path(data_dir);
    std::fs::write(
        &marker_path,
        b"Reset built-in Tor state on next wallet startup. Wallet data is not deleted.\n",
    )
    .wrap_err_with(|| format!("write Tor state reset marker {}", marker_path.display()))?;
    Ok(marker_path)
}

pub async fn build_wallet_network_context(config: WalletNetworkConfig<'_>) -> Result<HttpContext> {
    build_wallet_network_context_inner(config, None).await
}

pub async fn build_wallet_network_context_with_progress(
    config: WalletNetworkConfig<'_>,
    progress_tx: watch::Sender<WalletNetworkProgress>,
) -> Result<HttpContext> {
    build_wallet_network_context_inner(config, Some(progress_tx)).await
}

async fn build_wallet_network_context_inner(
    config: WalletNetworkConfig<'_>,
    progress_tx: Option<watch::Sender<WalletNetworkProgress>>,
) -> Result<HttpContext> {
    let mode = resolve_wallet_network_mode(config.network_mode, config.proxy)?;
    send_network_progress(
        progress_tx.as_ref(),
        WalletNetworkProgress::new(
            Some(mode),
            WalletNetworkProgressStage::ResolvingMode,
            Some(0),
            format!("Using {} network mode", mode.as_str()),
        ),
    );
    match mode {
        WalletNetworkMode::Tor => build_tor_context(config.data_dir, progress_tx.as_ref()).await,
        WalletNetworkMode::Proxy => {
            send_network_progress(
                progress_tx.as_ref(),
                WalletNetworkProgress::new(
                    Some(WalletNetworkMode::Proxy),
                    WalletNetworkProgressStage::ConfiguringNetwork,
                    Some(50),
                    "Configuring proxy-routed wallet HTTP client",
                ),
            );
            let context = build_reqwest_context(
                WalletNetworkMode::Proxy,
                config.proxy.cloned(),
                config.proxy.cloned(),
                None,
                None,
                None,
                None,
            )?;
            send_network_ready(progress_tx.as_ref(), &context);
            Ok(context)
        }
        WalletNetworkMode::Direct => {
            send_network_progress(
                progress_tx.as_ref(),
                WalletNetworkProgress::new(
                    Some(WalletNetworkMode::Direct),
                    WalletNetworkProgressStage::ConfiguringNetwork,
                    Some(50),
                    "Configuring direct wallet HTTP client",
                ),
            );
            let context = build_reqwest_context(
                WalletNetworkMode::Direct,
                None,
                None,
                None,
                None,
                None,
                None,
            )?;
            send_network_ready(progress_tx.as_ref(), &context);
            Ok(context)
        }
    }
}

pub fn resolve_wallet_network_mode(
    network_mode: Option<WalletNetworkMode>,
    proxy: Option<&Url>,
) -> Result<WalletNetworkMode> {
    match (network_mode, proxy) {
        (None, None) => Ok(WalletNetworkMode::Tor),
        (None | Some(WalletNetworkMode::Proxy), Some(_)) => Ok(WalletNetworkMode::Proxy),
        (Some(WalletNetworkMode::Proxy), None) => Err(eyre!(
            "--network-mode proxy requires --proxy <url> so proxy routing can fail closed"
        )),
        (Some(WalletNetworkMode::Tor), Some(_)) => Err(eyre!(
            "--network-mode tor cannot be combined with --proxy; omit --proxy to use built-in Tor"
        )),
        (Some(WalletNetworkMode::Direct), Some(_)) => Err(eyre!(
            "--network-mode direct cannot be combined with --proxy; remove --proxy or select --network-mode proxy"
        )),
        (Some(mode), None) => Ok(mode),
    }
}

async fn build_tor_context(
    data_dir: &Path,
    progress_tx: Option<&watch::Sender<WalletNetworkProgress>>,
) -> Result<HttpContext> {
    let arti_base = data_dir.join(ARTI_DIR);
    let state_dir = arti_base.join(ARTI_STATE_DIR);
    let cache_dir = arti_base.join(ARTI_CACHE_DIR);
    send_network_progress(
        progress_tx,
        WalletNetworkProgress::new(
            Some(WalletNetworkMode::Tor),
            WalletNetworkProgressStage::PreparingTorStorage,
            Some(5),
            format!("Preparing Arti state under {}", arti_base.display()),
        ),
    );
    consume_requested_tor_state_reset(data_dir, &arti_base)?;
    std::fs::create_dir_all(&state_dir)
        .wrap_err_with(|| format!("create Arti state directory {}", state_dir.display()))?;
    std::fs::create_dir_all(&cache_dir)
        .wrap_err_with(|| format!("create Arti cache directory {}", cache_dir.display()))?;

    tracing::info!(target: LOG_TARGET,
        state_dir = %state_dir.display(),
        cache_dir = %cache_dir.display(),
        "bootstrapping built-in Tor network context"
    );
    let tor_config = TorClientConfigBuilder::from_directories(&state_dir, &cache_dir)
        .build()
        .wrap_err("build Arti client config")?;
    send_network_progress(
        progress_tx,
        WalletNetworkProgress::new(
            Some(WalletNetworkMode::Tor),
            WalletNetworkProgressStage::BootstrappingTor,
            Some(10),
            "Starting Arti bootstrap",
        ),
    );
    let arti_client = TorClient::builder()
        .config(tor_config)
        .create_unbootstrapped_async()
        .await
        .wrap_err("create unbootstrapped Arti client")?;
    bootstrap_tor_client(&arti_client, progress_tx).await?;
    send_network_progress(
        progress_tx,
        WalletNetworkProgress::new(
            Some(WalletNetworkMode::Tor),
            WalletNetworkProgressStage::StartingTorBridge,
            Some(95),
            "Starting internal Arti SOCKS bridge",
        ),
    );
    let socks_bridge = Arc::new(
        ArtiSocksBridge::start(arti_client.clone(), 1)
            .await
            .wrap_err("start internal Arti SOCKS bridge")?,
    );
    let proxy_url = Url::parse(&format!("socks5h://{}", socks_bridge.local_addr()))
        .wrap_err("build internal Arti SOCKS proxy URL")?;

    tracing::info!(target: LOG_TARGET,proxy_url = %proxy_url, "built-in Tor network context ready");
    let context = build_reqwest_context(
        WalletNetworkMode::Tor,
        Some(proxy_url),
        None,
        Some(arti_client),
        Some(state_dir),
        Some(cache_dir),
        Some(socks_bridge),
    )?;
    send_network_ready(progress_tx, &context);
    Ok(context)
}

pub(super) fn tor_state_reset_marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TOR_STATE_RESET_MARKER_FILE)
}

pub(super) fn consume_requested_tor_state_reset(data_dir: &Path, arti_base: &Path) -> Result<()> {
    let marker_path = tor_state_reset_marker_path(data_dir);
    if !marker_path
        .try_exists()
        .wrap_err_with(|| format!("check Tor state reset marker {}", marker_path.display()))?
    {
        return Ok(());
    }

    tracing::warn!(target: LOG_TARGET,
        marker_path = %marker_path.display(),
        arti_dir = %arti_base.display(),
        "resetting built-in Tor state before startup"
    );
    if arti_base
        .try_exists()
        .wrap_err_with(|| format!("check Arti directory {}", arti_base.display()))?
    {
        std::fs::remove_dir_all(arti_base)
            .wrap_err_with(|| format!("remove Arti directory {}", arti_base.display()))?;
    }
    std::fs::remove_file(&marker_path)
        .wrap_err_with(|| format!("remove Tor state reset marker {}", marker_path.display()))?;
    Ok(())
}

async fn bootstrap_tor_client(
    arti_client: &WalletTorClient,
    progress_tx: Option<&watch::Sender<WalletNetworkProgress>>,
) -> Result<()> {
    let mut ticker = tokio::time::interval(TOR_BOOTSTRAP_PROGRESS_INTERVAL);
    let bootstrap = arti_client.bootstrap();
    tokio::pin!(bootstrap);
    loop {
        tokio::select! {
            result = &mut bootstrap => {
                result.wrap_err("bootstrap built-in Tor")?;
                send_network_progress(
                    progress_tx,
                    WalletNetworkProgress::new(
                        Some(WalletNetworkMode::Tor),
                        WalletNetworkProgressStage::BootstrappingTor,
                        Some(90),
                        "Tor bootstrap complete",
                    ),
                );
                return Ok(());
            }
            _ = ticker.tick() => {
                let status = arti_client.bootstrap_status();
                send_network_progress(
                    progress_tx,
                    WalletNetworkProgress::new(
                        Some(WalletNetworkMode::Tor),
                        WalletNetworkProgressStage::BootstrappingTor,
                        Some(tor_bootstrap_percent(status.as_frac())),
                        status.to_string(),
                    ),
                );
            }
        }
    }
}

fn send_network_ready(
    progress_tx: Option<&watch::Sender<WalletNetworkProgress>>,
    context: &HttpContext,
) {
    send_network_progress(
        progress_tx,
        WalletNetworkProgress::new(
            Some(context.network_mode()),
            WalletNetworkProgressStage::Ready,
            Some(100),
            context.network_status_detail(),
        ),
    );
}

fn send_network_progress(
    progress_tx: Option<&watch::Sender<WalletNetworkProgress>>,
    progress: WalletNetworkProgress,
) {
    if let Some(progress_tx) = progress_tx {
        let _ = progress_tx.send(progress);
    }
}

fn tor_bootstrap_percent(frac: f32) -> u8 {
    let raw = rounded_percent(frac);
    let scaled = 10_u16 + (u16::from(raw) * 80 / 100);
    u8::try_from(scaled).unwrap_or(90)
}

fn rounded_percent(frac: f32) -> u8 {
    let rounded = (frac.clamp(0.0, 1.0) * 100.0).round();
    let mut percent = 0_u8;
    while f32::from(percent) < rounded && percent < 100 {
        percent += 1;
    }
    percent
}

pub(super) fn build_reqwest_context(
    mode: WalletNetworkMode,
    proxy_url: Option<Url>,
    user_proxy_url: Option<Url>,
    arti_client: Option<WalletTorClient>,
    arti_state_dir: Option<PathBuf>,
    arti_cache_dir: Option<PathBuf>,
    socks_bridge: Option<Arc<ArtiSocksBridge>>,
) -> Result<HttpContext> {
    if let Some(proxy_url) = &proxy_url {
        let display_proxy_url = redact_url_for_display(proxy_url);
        tracing::info!(target: LOG_TARGET,network_mode = %mode, proxy_url = %display_proxy_url, "routing wallet HTTP traffic through proxy");
    }
    if mode == WalletNetworkMode::Direct {
        tracing::warn!(target: LOG_TARGET,
            "wallet direct network mode selected; outbound requests are not Tor-protected"
        );
    }
    let client = wallet_reqwest_client_builder(mode, proxy_url.as_ref())?
        .build()
        .wrap_err("build HTTP client")?;
    let rpc_client = wallet_reqwest_client_builder(mode, proxy_url.as_ref())?
        .timeout(rpc_request_timeout(mode))
        .build()
        .wrap_err("build RPC HTTP client")?;
    let direct_rpc_client = if mode == WalletNetworkMode::Direct {
        rpc_client.clone()
    } else {
        // `no_proxy` also keeps proxy environment variables from redirecting it.
        reqwest::Client::builder()
            .no_proxy()
            .referer(false)
            .timeout(rpc_request_timeout(WalletNetworkMode::Direct))
            .build()
            .wrap_err("build direct RPC HTTP client")?
    };
    let broker = RpcBroker::spawn(rpc_client.clone(), direct_rpc_client.clone())
        .wrap_err("start RPC broker")?;
    Ok(HttpContext {
        client,
        rpc_client,
        direct_rpc_client,
        broker,
        gateway_pool: GatewayPool::new(),
        proxy_url,
        user_proxy_url,
        mode,
        arti_client,
        arti_state_dir,
        arti_cache_dir,
        socks_bridge,
        operation_bridges: Arc::default(),
        fail_closed: mode != WalletNetworkMode::Direct,
        #[cfg(any(test, feature = "test-support"))]
        operation_clients_for_tests: None,
    })
}

pub(super) fn wallet_reqwest_client_builder(
    mode: WalletNetworkMode,
    proxy_url: Option<&Url>,
) -> Result<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder().referer(false);
    if let Some(proxy_url) = proxy_url {
        let display_proxy_url = redact_url_for_display(proxy_url);
        let proxy = reqwest::Proxy::all(proxy_url.as_str())
            .wrap_err_with(|| format!("invalid proxy URL {display_proxy_url}"))?;
        builder = builder.proxy(proxy);
    }
    if mode == WalletNetworkMode::Tor {
        builder = builder.pool_max_idle_per_host(0);
    }
    if mode != WalletNetworkMode::Direct {
        builder = builder
            .tls_backend_preconfigured(tls_config_without_resumption(&[b"h2", b"http/1.1"])?);
    }
    Ok(builder)
}

/// Reqwest's default rustls setup with session resumption off, so a server cannot link
/// connections that arrive over different Tor circuits or proxy exits.
pub(crate) fn tls_config_without_resumption(alpn: &[&[u8]]) -> Result<rustls::ClientConfig> {
    // Mirrors the `TlsBackend::Rustls` branch of reqwest 0.13's `ClientBuilder::build`.
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let verifier = rustls_platform_verifier::Verifier::new(Arc::clone(&provider))
        .wrap_err("load platform TLS verifier")?;
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::ALL_VERSIONS)
        .wrap_err("configure TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
    config.resumption = rustls::client::Resumption::disabled();
    Ok(config)
}

pub(super) const fn rpc_request_timeout(mode: WalletNetworkMode) -> Duration {
    match mode {
        WalletNetworkMode::Tor => TOR_RPC_REQUEST_TIMEOUT,
        WalletNetworkMode::Proxy | WalletNetworkMode::Direct => DIRECT_PROXY_RPC_REQUEST_TIMEOUT,
    }
}

pub(crate) fn redact_url_for_display(url: &Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    if !redacted.cannot_be_a_base() {
        redacted.set_path("");
    }
    redacted.set_query(None);
    redacted.set_fragment(None);
    redacted.to_string()
}
