use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

fn proxy_url() -> Url {
    Url::parse("socks5h://127.0.0.1:9050").expect("valid proxy URL")
}

fn sensitive_proxy_url() -> Url {
    Url::parse("socks5h://user:pass@example.com:9050/path?token=secret#fragment")
        .expect("valid sensitive proxy URL")
}

fn test_data_dir(name: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "wallet-ops-{name}-{}-{timestamp}",
        std::process::id()
    ))
}

#[test]
fn default_wallet_network_mode_is_tor() {
    assert_eq!(
        resolve_wallet_network_mode(None, None).expect("mode"),
        WalletNetworkMode::Tor
    );
}

#[test]
fn proxy_without_explicit_mode_implies_proxy_mode() {
    let proxy = proxy_url();
    assert_eq!(
        resolve_wallet_network_mode(None, Some(&proxy)).expect("mode"),
        WalletNetworkMode::Proxy
    );
}

#[test]
fn explicit_proxy_requires_proxy_url() {
    assert!(
        resolve_wallet_network_mode(Some(WalletNetworkMode::Proxy), None)
            .expect_err("proxy URL required")
            .to_string()
            .contains("requires --proxy")
    );
}

#[test]
fn proxy_conflicts_with_tor_and_direct_modes() {
    let proxy = proxy_url();
    assert!(resolve_wallet_network_mode(Some(WalletNetworkMode::Tor), Some(&proxy)).is_err());
    assert!(resolve_wallet_network_mode(Some(WalletNetworkMode::Direct), Some(&proxy)).is_err());
}

#[tokio::test]
async fn direct_network_health_is_ready() {
    let health = HttpContext::direct_for_tests().network_health();
    assert_eq!(health.mode, WalletNetworkMode::Direct);
    assert_eq!(health.state, WalletNetworkHealthState::Ready);
    assert_eq!(health.label(), "Direct mode");
    assert_eq!(health.cause, WalletNetworkHealthCause::None);
    assert!(
        HttpContext::direct_for_tests()
            .tor_bridge_activity_snapshot()
            .is_none()
    );
}

fn record_observation(
    tracker: &mut TorRuntimeHealthTracker,
    generation: u64,
    succeeded: bool,
    completed_at: Instant,
    setup_duration: Duration,
) -> bool {
    tracker.record_at(
        TorConnectionObservation::new(generation, succeeded, completed_at, setup_duration),
        completed_at,
    )
}

#[test]
fn runtime_health_rejects_wrong_generation_and_caps_recent_observations() {
    let start = Instant::now();
    let mut tracker = TorRuntimeHealthTracker::new(7);
    record_observation(&mut tracker, 8, true, start, Duration::from_millis(10));
    assert_eq!(
        tracker.observations(start),
        [] as [TorConnectionObservation; 0]
    );

    for index in 0..65 {
        let at = start + Duration::from_secs(index);
        record_observation(&mut tracker, 7, true, at, Duration::from_millis(10));
    }

    let observations = tracker.observations(start + Duration::from_secs(65));
    assert_eq!(observations.len(), TOR_OBSERVATION_LIMIT);
    assert_eq!(observations[0].completed_at, start + Duration::from_secs(1));
    assert!(
        observations
            .iter()
            .all(|observation| observation.generation == 7)
    );

    let at_exact_expiry = tracker.observations(start + Duration::from_secs(121));
    assert_eq!(at_exact_expiry.len(), TOR_OBSERVATION_LIMIT);
    assert_eq!(
        at_exact_expiry[0].completed_at,
        start + Duration::from_secs(1)
    );
    let after_expiry = tracker.observations(start + Duration::from_secs(122));
    assert_eq!(after_expiry.len(), TOR_OBSERVATION_LIMIT - 1);
    assert_eq!(after_expiry[0].completed_at, start + Duration::from_secs(2));
}

#[test]
fn runtime_health_preserves_equal_time_admission_order_for_failure_reset() {
    let start = Instant::now();
    let mut tracker = TorRuntimeHealthTracker::new(1);
    for index in 0..8 {
        record_observation(
            &mut tracker,
            1,
            true,
            start + Duration::from_secs(index),
            if index >= 5 {
                Duration::from_secs(8)
            } else {
                Duration::from_millis(1)
            },
        );
    }
    let equal_time = start + Duration::from_secs(8);
    for _ in 0..3 {
        record_observation(&mut tracker, 1, true, equal_time, Duration::from_millis(1));
    }
    record_observation(&mut tracker, 1, false, equal_time, Duration::ZERO);
    for _ in 0..3 {
        record_observation(&mut tracker, 1, true, equal_time, Duration::from_millis(1));
    }
    assert_eq!(tracker.health(equal_time), TorRuntimeHealth::Slow);

    record_observation(&mut tracker, 1, true, equal_time, Duration::from_millis(1));
    assert_eq!(tracker.health(equal_time), TorRuntimeHealth::Ready);
}

#[test]
fn runtime_health_requires_sufficient_evidence_and_ignores_isolated_failures() {
    let start = Instant::now();
    let mut insufficient = TorRuntimeHealthTracker::new(7);
    for index in 0..7 {
        record_observation(
            &mut insufficient,
            7,
            false,
            start + Duration::from_secs(index),
            Duration::from_millis(10),
        );
    }
    assert_eq!(
        insufficient.health(start + Duration::from_secs(7)),
        TorRuntimeHealth::Ready
    );

    let mut isolated = TorRuntimeHealthTracker::new(7);
    for index in 0..8 {
        record_observation(
            &mut isolated,
            7,
            index >= 3,
            start + Duration::from_secs(index),
            Duration::from_millis(10),
        );
    }
    assert_eq!(
        isolated.health(start + Duration::from_secs(8)),
        TorRuntimeHealth::Ready
    );

    let mut sustained = TorRuntimeHealthTracker::new(7);
    for index in 0..8 {
        record_observation(
            &mut sustained,
            7,
            false,
            start + Duration::from_secs(index),
            Duration::from_millis(10),
        );
    }
    assert_eq!(
        sustained.health(start + Duration::from_secs(8)),
        TorRuntimeHealth::Unreliable
    );
}

#[test]
fn runtime_health_does_not_credit_establishing_success_and_recovers_after_four_more() {
    let start = Instant::now();
    let mut tracker = TorRuntimeHealthTracker::new(1);
    for index in 0..8 {
        record_observation(
            &mut tracker,
            1,
            index >= 4,
            start + Duration::from_secs(index),
            Duration::from_millis(10),
        );
    }
    assert_eq!(
        tracker.health(start + Duration::from_secs(8)),
        TorRuntimeHealth::Unreliable
    );

    for index in 0..3 {
        let at = start + Duration::from_secs(u64::try_from(8 + index).expect("test index fits"));
        record_observation(&mut tracker, 1, true, at, Duration::from_millis(10));
        assert_eq!(tracker.health(at), TorRuntimeHealth::Unreliable);
    }
    let recovered_at = start + Duration::from_secs(11);
    record_observation(
        &mut tracker,
        1,
        true,
        recovered_at,
        Duration::from_millis(10),
    );
    assert_eq!(tracker.health(recovered_at), TorRuntimeHealth::Ready);
}

#[test]
fn runtime_health_detects_p75_setup_delay_and_preserves_slow_cause() {
    let start = Instant::now();
    let mut tracker = TorRuntimeHealthTracker::new(1);
    for index in 0..8 {
        record_observation(
            &mut tracker,
            1,
            true,
            start + Duration::from_secs(index),
            if index >= 5 {
                Duration::from_secs(8)
            } else {
                Duration::from_millis(1)
            },
        );
    }
    let failed_at = start + Duration::from_secs(8);
    record_observation(&mut tracker, 1, false, failed_at, Duration::ZERO);
    let report = tracker.health_report(failed_at);
    assert_eq!(report.health, TorRuntimeHealth::Slow);
    assert_eq!(report.health, tracker.classify(failed_at));
    assert_eq!(report.recent_successful_sample_count, 8);
    assert_eq!(
        report.recent_successful_setup_p75,
        Some(Duration::from_secs(8))
    );

    for index in 0..3 {
        let at = start + Duration::from_secs(9 + index);
        record_observation(&mut tracker, 1, true, at, Duration::from_millis(1));
        assert_eq!(tracker.health(at), TorRuntimeHealth::Slow);
    }
    let recovered_at = start + Duration::from_secs(12);
    record_observation(
        &mut tracker,
        1,
        true,
        recovered_at,
        Duration::from_millis(1),
    );
    assert_eq!(tracker.health(recovered_at), TorRuntimeHealth::Ready);
}

#[test]
fn runtime_health_expires_stale_observations_and_resets_success_streak() {
    let start = Instant::now();
    let mut tracker = TorRuntimeHealthTracker::new(1);
    for index in 0..8 {
        record_observation(
            &mut tracker,
            1,
            index < 4,
            start + Duration::from_secs(index),
            Duration::from_millis(1),
        );
    }
    assert_eq!(
        tracker.health(start + Duration::from_secs(8)),
        TorRuntimeHealth::Unreliable
    );
    let stale_at = start + Duration::from_secs(200);
    assert_eq!(tracker.health(stale_at), TorRuntimeHealth::Ready);
    assert_eq!(
        tracker.observations(stale_at),
        [] as [TorConnectionObservation; 0]
    );

    for index in 0..3 {
        let at = stale_at + Duration::from_secs(index);
        record_observation(&mut tracker, 1, true, at, Duration::from_millis(1));
    }
    assert_eq!(
        tracker.health(stale_at + Duration::from_secs(3)),
        TorRuntimeHealth::Ready
    );
}

#[test]
fn runtime_health_clamps_credited_successes_after_partial_expiry() {
    let start = Instant::now();
    let mut tracker = TorRuntimeHealthTracker::new(1);
    for index in 0..8 {
        let at = start + Duration::from_secs(100 + index);
        record_observation(&mut tracker, 1, true, at, Duration::from_secs(8));
    }
    for index in 108..=110 {
        let at = start + Duration::from_secs(index);
        record_observation(&mut tracker, 1, true, at, Duration::from_millis(1));
    }

    let after_expiry = start + Duration::from_secs(229);
    let retained = tracker.observations(after_expiry);
    assert_eq!(retained.len(), 2);
    assert_eq!(tracker.health(after_expiry), TorRuntimeHealth::Slow);
    assert_ne!(
        tracker.observations(after_expiry),
        [] as [TorConnectionObservation; 0]
    );

    let first_fresh_success = after_expiry;
    record_observation(
        &mut tracker,
        1,
        true,
        first_fresh_success,
        Duration::from_millis(1),
    );
    assert_eq!(tracker.health(first_fresh_success), TorRuntimeHealth::Slow);

    let second_fresh_success = after_expiry;
    record_observation(
        &mut tracker,
        1,
        true,
        second_fresh_success,
        Duration::from_millis(1),
    );
    assert_eq!(
        tracker.health(second_fresh_success),
        TorRuntimeHealth::Ready
    );
}

#[test]
fn runtime_health_updates_current_cause_through_ready_hysteresis() {
    let start = Instant::now();
    let mut tracker = TorRuntimeHealthTracker::new(1);
    for index in 0..8 {
        record_observation(
            &mut tracker,
            1,
            true,
            start + Duration::from_secs(200 + index),
            Duration::from_secs(8),
        );
    }
    let keep_alive_at = start + Duration::from_secs(210);
    record_observation(&mut tracker, 1, true, keep_alive_at, Duration::from_secs(8));
    assert_eq!(tracker.health(keep_alive_at), TorRuntimeHealth::Slow);

    let ready_at = start + Duration::from_secs(330);
    assert_eq!(tracker.health(ready_at), TorRuntimeHealth::Slow);

    for index in 0..8 {
        let at = ready_at + Duration::from_secs(index);
        record_observation(&mut tracker, 1, false, at, Duration::ZERO);
    }
    assert_eq!(
        tracker.health(ready_at + Duration::from_secs(7)),
        TorRuntimeHealth::Unreliable
    );

    for index in 0..8 {
        let at = ready_at + Duration::from_secs(8 + index);
        record_observation(&mut tracker, 1, true, at, Duration::from_secs(8));
    }
    let boundary_report = tracker.health_report(ready_at + Duration::from_secs(15));
    assert_eq!(boundary_report.health, TorRuntimeHealth::Unreliable);
    assert_eq!(boundary_report.recent_failed_attempt_count, 8);
    assert_eq!(boundary_report.recent_attempt_count, 16);

    let next_at = ready_at + Duration::from_secs(16);
    record_observation(&mut tracker, 1, true, next_at, Duration::from_secs(8));
    let report = tracker.health_report(next_at);
    assert_eq!(report.health, TorRuntimeHealth::Slow);
    assert_eq!(report.recent_successful_sample_count, 9);
}

#[tokio::test]
async fn proxy_network_health_is_ready() {
    let proxy = proxy_url();
    let context = build_reqwest_context(
        WalletNetworkMode::Proxy,
        Some(proxy.clone()),
        Some(proxy),
        None,
        None,
        None,
        None,
    )
    .expect("proxy context");
    let health = context.network_health();
    assert_eq!(health.mode, WalletNetworkMode::Proxy);
    assert_eq!(health.state, WalletNetworkHealthState::Ready);
    assert_eq!(health.label(), "Proxy mode");
}

#[test]
fn proxy_url_display_redacts_credentials_query_and_fragment() {
    let redacted = redact_url_for_display(&sensitive_proxy_url());

    assert_eq!(redacted, "socks5h://example.com:9050");
    assert!(!redacted.contains("user"));
    assert!(!redacted.contains("pass"));
    assert!(!redacted.contains("path"));
    assert!(!redacted.contains("token"));
    assert!(!redacted.contains("fragment"));
}

async fn serve_one_http_request(listener: TcpListener, response: String) -> String {
    let (mut stream, _) = listener.accept().await.expect("accept HTTP request");
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    while !request.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut buffer).await.expect("read HTTP request");
        assert_ne!(read, 0, "client closed before sending headers");
        request.extend_from_slice(&buffer[..read]);
    }
    stream
        .write_all(response.as_bytes())
        .await
        .expect("write HTTP response");
    String::from_utf8(request).expect("UTF-8 HTTP request")
}

#[tokio::test]
async fn wallet_client_follows_cross_origin_redirect_without_referer() {
    let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind target");
    let target_url = format!("http://{}/end", target.local_addr().unwrap());
    let start = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind start");
    let start_url = format!("http://{}/start?key=secret", start.local_addr().unwrap());
    tokio::spawn(serve_one_http_request(
        start,
        format!("HTTP/1.1 302 Found\r\nLocation: {target_url}\r\nContent-Length: 0\r\n\r\n"),
    ));
    let target_request = tokio::spawn(serve_one_http_request(
        target,
        "HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nsecond".to_owned(),
    ));
    let context = build_reqwest_context(
        WalletNetworkMode::Direct,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("direct context");

    let body = context
        .client
        .get(start_url)
        .send()
        .await
        .expect("follow redirect")
        .text()
        .await
        .expect("read body");

    assert_eq!(body, "second");
    let target_request = target_request.await.unwrap().to_ascii_lowercase();
    assert!(target_request.starts_with("get /end "));
    assert!(!target_request.contains("referer:"), "{target_request}");
}

#[tokio::test]
async fn proxied_tls_config_never_resumes_and_keeps_platform_verification() {
    use rustls::HandshakeKind;
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};

    async fn connect(config: &Arc<rustls::ClientConfig>, addr: SocketAddr) -> io::Result<()> {
        let stream = TcpStream::connect(addr).await?;
        let mut tls = tokio_rustls::TlsConnector::from(Arc::clone(config))
            .connect(ServerName::try_from("localhost").unwrap(), stream)
            .await?;
        // TLS 1.3 tickets arrive after the handshake, ahead of this byte.
        tls.read_exact(&mut [0; 1]).await?;
        Ok(())
    }

    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("test certificate");
    let cert = certified.cert.der().clone();
    let server_config = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der())),
        )
        .expect("server config");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind TLS server");
    let addr = listener.local_addr().unwrap();
    let (kinds_tx, mut kinds_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept TLS client");
            let Ok(mut tls) = acceptor.accept(stream).await else {
                continue;
            };
            let kind = tls.get_ref().1.handshake_kind();
            tls.write_all(b"x").await.expect("write TLS byte");
            tls.flush().await.expect("flush TLS byte");
            kinds_tx.send(kind).expect("report handshake kind");
        }
    });

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).expect("trust test certificate");
    let test_root_verifier =
        rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
            .build()
            .expect("test verifier");
    let mut without_resumption = tls_config_without_resumption(&[b"http/1.1"]).unwrap();
    without_resumption
        .dangerous()
        .set_certificate_verifier(test_root_verifier);
    let mut control = without_resumption.clone();
    control.resumption = rustls::client::Resumption::default();

    let without_resumption = Arc::new(without_resumption);
    for _ in 0..3 {
        connect(&without_resumption, addr).await.expect("connect");
        assert_eq!(kinds_rx.recv().await.unwrap(), Some(HandshakeKind::Full));
    }
    let control = Arc::new(control);
    connect(&control, addr).await.expect("control connect");
    assert_eq!(kinds_rx.recv().await.unwrap(), Some(HandshakeKind::Full));
    connect(&control, addr).await.expect("control reconnect");
    assert_eq!(kinds_rx.recv().await.unwrap(), Some(HandshakeKind::Resumed));

    let unmodified = Arc::new(tls_config_without_resumption(&[b"http/1.1"]).unwrap());
    let error = connect(&unmodified, addr)
        .await
        .expect_err("platform verifier rejects the test certificate");
    assert!(
        matches!(
            error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<rustls::Error>()),
            Some(rustls::Error::InvalidCertificate(_))
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn proxy_network_health_redacts_configured_proxy_url() {
    let proxy = sensitive_proxy_url();
    let context = build_reqwest_context(
        WalletNetworkMode::Proxy,
        Some(proxy.clone()),
        Some(proxy),
        None,
        None,
        None,
        None,
    )
    .expect("proxy context");
    let detail = context.network_status_detail();

    assert!(detail.contains("socks5h://example.com:9050"));
    assert!(!detail.contains("user"));
    assert!(!detail.contains("pass"));
    assert!(!detail.contains("path"));
    assert!(!detail.contains("token"));
    assert!(!detail.contains("fragment"));
}

#[tokio::test]
async fn tor_network_health_without_client_is_degraded() {
    let context = build_reqwest_context(
        WalletNetworkMode::Tor,
        Some(proxy_url()),
        None,
        None,
        None,
        None,
        None,
    )
    .expect("tor context");
    let health = context.network_health();
    assert_eq!(health.mode, WalletNetworkMode::Tor);
    assert_eq!(health.state, WalletNetworkHealthState::Degraded);
    assert_eq!(health.label(), "Tor degraded");
    assert!(health.detail.contains("unavailable"));
}

#[tokio::test]
async fn start_new_tor_session_requires_tor_mode() {
    let error = HttpContext::direct_for_tests()
        .start_new_tor_session()
        .expect_err("direct mode cannot start Tor sessions");
    assert!(error.to_string().contains("only available in Tor mode"));
}

#[tokio::test]
async fn start_new_tor_session_requires_internal_socks_bridge() {
    let context = build_reqwest_context(
        WalletNetworkMode::Tor,
        Some(proxy_url()),
        None,
        None,
        None,
        None,
        None,
    )
    .expect("tor context");
    assert_eq!(context.tor_session_generation(), 0);
    let error = context
        .start_new_tor_session()
        .expect_err("Tor sessions require the internal SOCKS bridge");
    assert!(error.to_string().contains("internal SOCKS bridge"));
}

#[tokio::test]
async fn operation_isolation_is_unavailable_outside_built_in_tor() {
    let direct = HttpContext::direct_for_tests()
        .operation_http_client()
        .await
        .expect("direct operation client");
    assert_eq!(
        direct.isolation(),
        OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct)
    );

    let proxy = proxy_url();
    let proxy_context = build_reqwest_context(
        WalletNetworkMode::Proxy,
        Some(proxy.clone()),
        Some(proxy),
        None,
        None,
        None,
        None,
    )
    .expect("proxy context");
    let proxied = proxy_context
        .operation_http_client()
        .await
        .expect("proxy operation client");
    assert_eq!(
        proxied.isolation(),
        OperationNetworkIsolation::Unavailable(WalletNetworkMode::Proxy)
    );

    let tor_without_bridge = build_reqwest_context(
        WalletNetworkMode::Tor,
        Some(proxy_url()),
        None,
        None,
        None,
        None,
        None,
    )
    .expect("tor context");
    assert!(
        tor_without_bridge.operation_http_client().await.is_err(),
        "Tor mode without its bridge fails closed"
    );
}

#[tokio::test]
async fn direct_rpc_client_bypasses_tor_and_proxy() {
    use std::sync::atomic::AtomicUsize;

    let answered = Arc::new(AtomicUsize::new(0));
    let (rpc, rpc_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new({
            let answered = answered.clone();
            move |request: serde_json::Value| {
                answered.fetch_add(1, Ordering::SeqCst);
                serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x1"})
            }
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    // Stands in for the Tor bridge or proxy: counts connections and closes them unanswered.
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let proxy_address = proxy.local_addr().unwrap();
    let proxied = Arc::new(AtomicUsize::new(0));
    let proxy_server = tokio::spawn({
        let proxied = proxied.clone();
        async move {
            while let Ok((stream, _)) = proxy.accept().await {
                proxied.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        }
    });
    let send = |client: &reqwest::Client| {
        client
            .post(rpc.clone())
            .header("content-type", "application/json")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}"#)
            .send()
    };

    for (mode, scheme) in [
        (WalletNetworkMode::Tor, "socks5h"),
        (WalletNetworkMode::Proxy, "http"),
    ] {
        let proxy_url = Url::parse(&format!("{scheme}://{proxy_address}")).unwrap();
        let user_proxy_url = (mode == WalletNetworkMode::Proxy).then(|| proxy_url.clone());
        let context = build_reqwest_context(
            mode,
            Some(proxy_url),
            user_proxy_url,
            None,
            None,
            None,
            None,
        )
        .expect("context");
        let proxied_before = proxied.load(Ordering::SeqCst);
        assert!(
            send(context.rpc_client_for(RpcTransport::Direct))
                .await
                .is_ok()
        );
        assert_eq!(proxied.load(Ordering::SeqCst), proxied_before);
        assert!(
            send(context.rpc_client_for(RpcTransport::Wallet))
                .await
                .is_err()
        );
        assert!(proxied.load(Ordering::SeqCst) > proxied_before);
    }
    assert_eq!(answered.load(Ordering::SeqCst), 2);

    let direct = build_reqwest_context(
        WalletNetworkMode::Direct,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("direct context");
    for transport in [RpcTransport::Wallet, RpcTransport::Direct] {
        assert!(send(direct.rpc_client_for(transport)).await.is_ok());
    }
    assert_eq!(answered.load(Ordering::SeqCst), 4);
    rpc_server.abort();
    proxy_server.abort();
}

#[test]
fn tor_operations_get_their_own_bridge_on_the_shared_tor_instance() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let data_dir = test_data_dir("operation-isolation");
    let arti_dir = data_dir.join(ARTI_DIR);
    let state_dir = arti_dir.join(ARTI_STATE_DIR);
    let cache_dir = arti_dir.join(ARTI_CACHE_DIR);
    std::fs::create_dir_all(&state_dir).expect("create Arti state directory");
    std::fs::create_dir_all(&cache_dir).expect("create Arti cache directory");

    runtime.block_on(async {
        let tor_config = TorClientConfigBuilder::from_directories(&state_dir, &cache_dir)
            .build()
            .expect("build Arti client config");
        let arti_client = TorClient::builder()
            .config(tor_config)
            .create_unbootstrapped_async()
            .await
            .expect("create unbootstrapped Arti client");
        let shared_bridge = Arc::new(
            ArtiSocksBridge::start(arti_client.clone(), 1)
                .await
                .expect("start shared Arti SOCKS bridge"),
        );
        let shared_addr = shared_bridge.local_addr();
        let proxy_url = Url::parse(&format!("socks5h://{shared_addr}")).expect("shared bridge URL");
        let context = build_reqwest_context(
            WalletNetworkMode::Tor,
            Some(proxy_url),
            None,
            Some(arti_client),
            Some(state_dir),
            Some(cache_dir),
            Some(shared_bridge),
        )
        .expect("tor context");

        let first = context
            .operation_http_client()
            .await
            .expect("first operation client");
        let second = context
            .operation_http_client()
            .await
            .expect("second operation client");

        assert_eq!(first.isolation(), OperationNetworkIsolation::Dedicated);
        assert_eq!(second.isolation(), OperationNetworkIsolation::Dedicated);
        let first_addr = first.bridge().expect("first operation bridge").local_addr();
        let second_addr = second
            .bridge()
            .expect("second operation bridge")
            .local_addr();
        assert!(first_addr.ip().is_loopback());
        assert_ne!(first_addr, shared_addr);
        assert_ne!(second_addr, shared_addr);
        assert_ne!(first_addr, second_addr);
        // Operation isolation leaves the shared sync, RPC, and broadcaster session alone.
        assert_eq!(context.tor_session_generation(), 1);

        // A new session moves both open operations onto fresh clients of their own.
        let session = |client: &OperationHttpClient| {
            ArtiSocksBridge::capture_session(&client.bridge().expect("bridge").session)
                .expect("capture operation session")
        };
        let before = [session(&first).client, session(&second).client];
        assert_eq!(context.start_new_tor_session().expect("new session"), 2);
        let [first_session, second_session] = [session(&first), session(&second)];
        assert_eq!(first_session.generation, 2);
        assert_eq!(second_session.generation, 2);
        let shared_client = context.arti_client().expect("shared client");
        let clients = [
            &before[0],
            &before[1],
            &first_session.client,
            &second_session.client,
        ];
        for (index, client) in clients.iter().enumerate() {
            assert!(!Arc::ptr_eq(client, &shared_client));
            for other in &clients[index + 1..] {
                assert!(!Arc::ptr_eq(client, other));
            }
        }

        // Re-isolation gives one operation new circuits in the same session.
        let provider = first.tor_client_provider().expect("first provider");
        first.reisolate().expect("reisolate first operation");
        let reisolated = provider().expect("reisolated client");
        assert!(!Arc::ptr_eq(&reisolated, &first_session.client));
        assert_eq!(session(&first).generation, 2);
        assert!(Arc::ptr_eq(
            &session(&second).client,
            &second_session.client
        ));
        assert_eq!(context.start_new_tor_session().expect("new session"), 3);
        assert_eq!(session(&first).generation, 3);

        // A raw client clone keeps the bridge running after its operation client drops.
        let rpc_client = first.rpc_client().clone();
        drop(first);
        TcpStream::connect(first_addr)
            .await
            .expect("a client clone keeps the operation bridge running");
        drop(rpc_client);
        let mut stopped = false;
        for _ in 0..100 {
            if TcpStream::connect(first_addr).await.is_err() {
                stopped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(stopped, "the bridge stops with the last client clone");
    });
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn socks_accept_error_retry_continues_without_shutdown() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (_shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let should_continue = tokio::time::timeout(
            SOCKS_ACCEPT_ERROR_BACKOFF * 2,
            wait_after_socks_accept_error(&mut shutdown_rx),
        )
        .await
        .expect("accept error backoff returns");
        assert!(should_continue);
    });
}

#[test]
fn socks_accept_error_retry_stops_on_shutdown() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        shutdown_tx.send(true).expect("send shutdown");
        let should_continue = tokio::time::timeout(
            SOCKS_ACCEPT_ERROR_BACKOFF * 2,
            wait_after_socks_accept_error(&mut shutdown_rx),
        )
        .await
        .expect("shutdown returns");
        assert!(!should_continue);
    });
}

#[test]
fn request_tor_state_reset_creates_marker() {
    let data_dir = test_data_dir("reset-marker");
    let marker = request_tor_state_reset(&data_dir).expect("request Tor reset");
    assert_eq!(marker, tor_state_reset_marker_path(&data_dir));
    let marker_text = std::fs::read_to_string(&marker).expect("read marker");
    assert!(marker_text.contains("Reset built-in Tor state"));
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn consume_requested_tor_state_reset_removes_only_arti_and_marker() {
    let data_dir = test_data_dir("reset-consume");
    let arti_base = data_dir.join(ARTI_DIR);
    let wallet_file = data_dir.join("wallet.db");
    std::fs::create_dir_all(arti_base.join(ARTI_STATE_DIR)).expect("create Arti state");
    std::fs::write(arti_base.join(ARTI_STATE_DIR).join("state"), b"state")
        .expect("write Arti state");
    std::fs::write(&wallet_file, b"wallet").expect("write wallet file");
    let marker = request_tor_state_reset(&data_dir).expect("request Tor reset");

    consume_requested_tor_state_reset(&data_dir, &arti_base).expect("consume Tor reset");

    assert!(!marker.exists());
    assert!(!arti_base.exists());
    assert_eq!(
        std::fs::read(&wallet_file).expect("read wallet file"),
        b"wallet"
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn relay_socks_stream_counts_only_downloads_and_releases_active_stream() {
    let activity = TorBridgeActivity::new(1);
    let active_stream = activity.begin_connection().finish_success();

    let (mut client, inbound) = tokio::io::duplex(64);
    let (outbound, mut tor) = tokio::io::duplex(64);
    let relay = tokio::spawn(relay_socks_stream(inbound, outbound, active_stream));

    let upload = b"client-to-tor";
    client
        .write_all(upload)
        .await
        .expect("write client payload");
    let mut received_upload = vec![0_u8; upload.len()];
    tor.read_exact(&mut received_upload)
        .await
        .expect("read client payload on Tor side");
    assert_eq!(received_upload, upload);

    let after_upload = activity.snapshot();
    assert_eq!(after_upload.downloaded_bytes, 0);
    assert_eq!(after_upload.active_streams, 1);

    let download = b"tor-to-client";
    tor.write_all(download).await.expect("write Tor payload");
    let mut received_download = vec![0_u8; download.len()];
    client
        .read_exact(&mut received_download)
        .await
        .expect("read Tor payload on client side");
    assert_eq!(received_download, download);
    assert!(!relay.is_finished());

    let while_open = activity.snapshot();
    assert_eq!(while_open.downloaded_bytes, download.len() as u64);
    assert_eq!(while_open.active_streams, 1);

    client.shutdown().await.expect("shutdown client side");
    tor.shutdown().await.expect("shutdown Tor side");
    relay
        .await
        .expect("relay task panicked")
        .expect("relay failed");

    let after_relay = activity.snapshot();
    assert_eq!(after_relay.downloaded_bytes, download.len() as u64);
    assert_eq!(after_relay.active_streams, 0);
}

#[test]
fn activity_snapshot_reports_recent_samples_and_preserves_cumulative_counts() {
    let activity = TorBridgeActivity::new(7);
    let origin = activity.activity_origin;
    activity.downloaded_bytes.store(23, Ordering::Relaxed);
    activity.successful_connections.store(2, Ordering::Relaxed);
    activity.failed_connections.store(1, Ordering::Relaxed);
    activity.last_activity_tick.store(
        u64::try_from(Duration::from_secs(15).as_nanos()).expect("activity tick fits"),
        Ordering::Relaxed,
    );

    {
        let mut tracker = activity.tracker.lock().expect("lock activity tracker");
        record_observation(
            &mut tracker,
            7,
            true,
            origin + Duration::from_secs(10),
            Duration::from_secs(2),
        );
        record_observation(
            &mut tracker,
            7,
            false,
            origin + Duration::from_secs(12),
            Duration::ZERO,
        );
        record_observation(
            &mut tracker,
            7,
            true,
            origin + Duration::from_secs(14),
            Duration::from_secs(4),
        );
    }

    let snapshot = activity.snapshot_at(origin + Duration::from_secs(20));
    assert_eq!(snapshot.generation, 7);
    assert_eq!(snapshot.session_duration, Duration::from_secs(20));
    assert_eq!(snapshot.downloaded_bytes, 23);
    assert_eq!(snapshot.successful_connections, 2);
    assert_eq!(snapshot.failed_connections, 1);
    assert_eq!(snapshot.recent_connection_sample_count, 3);
    assert_eq!(snapshot.recent_successful_sample_count, 2);
    assert_eq!(snapshot.median_setup_duration, Some(Duration::from_secs(3)));
    assert_eq!(snapshot.last_activity_age, Some(Duration::from_secs(5)));

    let stale_snapshot = activity.snapshot_at(origin + Duration::from_secs(135));
    assert_eq!(stale_snapshot.session_duration, Duration::from_secs(135));
    assert_eq!(stale_snapshot.recent_connection_sample_count, 0);
    assert_eq!(stale_snapshot.recent_successful_sample_count, 0);
    assert_eq!(stale_snapshot.median_setup_duration, None);
    assert_eq!(stale_snapshot.successful_connections, 2);
    assert_eq!(stale_snapshot.failed_connections, 1);
}

#[test]
fn activity_guards_clean_up_connecting_and_active_streams() {
    let activity = TorBridgeActivity::new(1);

    {
        let connecting = activity.begin_connection();
        assert_eq!(activity.snapshot().connecting_streams, 1);
        drop(connecting);
    }
    assert_eq!(activity.snapshot().connecting_streams, 0);
    assert_eq!(activity.snapshot().successful_connections, 0);
    assert_eq!(activity.snapshot().failed_connections, 0);

    let active = activity.begin_connection().finish_success();
    let snapshot = activity.snapshot();
    assert_eq!(snapshot.connecting_streams, 0);
    assert_eq!(snapshot.active_streams, 1);
    assert_eq!(snapshot.successful_connections, 1);
    drop(active);
    assert_eq!(activity.snapshot().active_streams, 0);

    activity.begin_connection().finish_failure();
    let snapshot = activity.snapshot();
    assert_eq!(snapshot.connecting_streams, 0);
    assert_eq!(snapshot.active_streams, 0);
    assert_eq!(snapshot.failed_connections, 1);
}

#[test]
fn isolated_session_replaces_real_bridge_activity_without_cross_contamination() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let data_dir = test_data_dir("bridge-session");
    let arti_dir = data_dir.join(ARTI_DIR);
    let state_dir = arti_dir.join(ARTI_STATE_DIR);
    let cache_dir = arti_dir.join(ARTI_CACHE_DIR);
    std::fs::create_dir_all(&state_dir).expect("create Arti state directory");
    std::fs::create_dir_all(&cache_dir).expect("create Arti cache directory");

    let result: Result<()> = runtime.block_on(async {
        let tor_config = TorClientConfigBuilder::from_directories(&state_dir, &cache_dir)
            .build()
            .expect("build Arti client config");
        let arti_client = TorClient::builder()
            .config(tor_config)
            .create_unbootstrapped_async()
            .await
            .expect("create unbootstrapped Arti client");
        let bridge = ArtiSocksBridge::start(arti_client, 1)
            .await
            .expect("start loopback Arti SOCKS bridge");
        assert!(bridge.local_addr().ip().is_loopback());

        let old_capture = ArtiSocksBridge::capture_session(&bridge.session)
            .expect("capture initial bridge session");
        assert_eq!(old_capture.generation, 1);
        assert_eq!(old_capture.generation, old_capture.activity.generation);
        let old_connection = old_capture.activity.begin_connection();

        let new_generation = bridge
            .new_isolated_session()
            .expect("replace bridge session with isolated client");
        assert_eq!(new_generation, old_capture.generation + 1);
        let new_capture = ArtiSocksBridge::capture_session(&bridge.session)
            .expect("capture replacement bridge session");
        assert_eq!(new_capture.generation, new_generation);
        assert_eq!(new_capture.generation, new_capture.activity.generation);
        assert!(!Arc::ptr_eq(&old_capture.activity, &new_capture.activity));
        let fresh_snapshot = bridge
            .activity_snapshot()
            .expect("snapshot replacement activity");
        assert_eq!(fresh_snapshot.generation, new_generation);
        assert_eq!(fresh_snapshot.downloaded_bytes, 0);
        assert_eq!(fresh_snapshot.connecting_streams, 0);
        assert_eq!(fresh_snapshot.active_streams, 0);
        assert_eq!(fresh_snapshot.successful_connections, 0);
        assert_eq!(fresh_snapshot.failed_connections, 0);
        assert_eq!(new_capture.activity.snapshot().generation, new_generation);

        let old_active = old_connection.finish_success();
        old_capture.activity.add_downloaded_bytes(8);
        drop(old_active);
        let old_snapshot = old_capture.activity.snapshot();
        assert_eq!(old_snapshot.downloaded_bytes, 8);
        assert_eq!(old_snapshot.connecting_streams, 0);
        assert_eq!(old_snapshot.active_streams, 0);
        assert_eq!(old_snapshot.successful_connections, 1);
        assert_eq!(new_capture.activity.snapshot().downloaded_bytes, 0);
        assert_eq!(new_capture.activity.snapshot().connecting_streams, 0);
        assert_eq!(new_capture.activity.snapshot().active_streams, 0);
        assert_eq!(new_capture.activity.snapshot().successful_connections, 0);

        let current_snapshot = bridge
            .activity_snapshot()
            .expect("snapshot current activity after old completion");
        assert_eq!(current_snapshot.generation, new_generation);
        assert_eq!(current_snapshot.downloaded_bytes, 0);
        assert_eq!(current_snapshot.connecting_streams, 0);
        assert_eq!(current_snapshot.active_streams, 0);
        assert_eq!(current_snapshot.successful_connections, 0);
        assert_eq!(current_snapshot.failed_connections, 0);
        assert_eq!(current_snapshot.recent_connection_sample_count, 0);
        assert_eq!(current_snapshot.recent_successful_sample_count, 0);
        assert_eq!(current_snapshot.median_setup_duration, None);
        assert_eq!(current_snapshot.last_activity_age, None);
        Ok(())
    });
    result.expect("bridge session replacement test");
    let _ = std::fs::remove_dir_all(&data_dir);
}
