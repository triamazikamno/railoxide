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
        blinded_commitment: "0x0000000000000000000000000000000000000000000000000000000000000000"
            .to_string(),
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
        spent_block_timestamp: None,
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
    let error = tokio::time::timeout(Duration::from_secs(10), source.ensure_poi_artifacts(3, 3))
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
                        relay_adapt_contract: None,
                        relay_adapt_history: &[],
                        relay_adapt_7702_contract: Address::ZERO,
                        relay_adapt_7702_delegates: &[],
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
                    archive_rpc_http_client: None,
                    block_time: Duration::from_secs(12),
                    finality_depth: 0,
                    http_client: reqwest::Client::new(),
                    progress_tx: None,
                    poi_submission_routes: None,
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

#[tokio::test]
async fn chain_config_isolates_ppoi_submissions_only_in_built_in_tor_mode() {
    let effective_chain =
        settings::build_effective_chain_configs(&settings::WalletSettings::default())
            .expect("default chain configs")
            .get(1)
            .cloned()
            .expect("Ethereum chain config");
    for (mode, isolated) in [
        (WalletNetworkMode::Tor, true),
        (WalletNetworkMode::Proxy, false),
        (WalletNetworkMode::Direct, false),
    ] {
        let http = HttpContext::with_rpc_client_for_tests(reqwest::Client::new(), mode);
        let config = chain_config(&effective_chain, &http, None).expect("chain config");
        assert_eq!(config.poi_submission_routes.is_some(), isolated, "{mode:?}");
    }
}

#[tokio::test]
async fn sync_pool_leaves_out_endpoints_without_the_general_flag() {
    let mut wallet_settings = settings::WalletSettings::default();
    wallet_settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("Ethereum settings")
        .rpc_endpoints = vec![
        settings::RpcEndpointSetting {
            url: "https://node.example".to_owned(),
            general: false,
            ..settings::RpcEndpointSetting::default()
        },
        "https://general.example".to_owned().into(),
    ];
    let effective_chain = settings::build_effective_chain_configs(&wallet_settings)
        .expect("chain configs")
        .get(1)
        .cloned()
        .expect("Ethereum chain config");
    let config = chain_config(&effective_chain, &HttpContext::direct_for_tests(), None)
        .expect("chain config");
    let urls = config
        .rpcs
        .available_providers()
        .into_iter()
        .map(|provider| provider.url)
        .collect::<Vec<_>>();
    assert_eq!(
        urls,
        vec![url::Url::parse("https://general.example").unwrap()]
    );
}
