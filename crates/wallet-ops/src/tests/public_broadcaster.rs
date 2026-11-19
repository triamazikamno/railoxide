use super::helpers::*;
use alloy::rpc::types::TransactionRequest;
use railgun_wallet::tx::{ETHEREUM_GAS_MODEL, GasEstimateMode, POLYGON_GAS_MODEL};

/// Submissions on fake submission nodes, which report what they were built from and did.
mod submission_nodes {
    use super::*;
    use crate::desktop::{
        PublicBroadcasterDelivery, SubmissionNode, SubmissionNodeSpec, SubmissionRouteHealth,
        submit_public_broadcaster_transaction,
    };
    use crate::http::{OperationHttpClient, OperationNetworkIsolation};
    use crate::rpc_broker::tests::{spawn_recording_rpc_mock, tagged_reqwest_client};
    use crate::{
        HttpContext, PublicBroadcasterWaku, SubmissionNodeScope, SubmissionRouteProgress,
        SubmissionRouteSender, SubmissionRouteState, WalletNetworkMode, WalletTorClientProvider,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::sync::{mpsc, watch};
    use tokio::time::Instant;
    use waku::proto::WakuMessage;
    use waku_relay::client::{
        AdditionalPeer, Client as WakuClient, ClientConfig, RelayNetworkConfig, RelayNetworkMode,
    };

    const RESPONSE_TOPIC: &str = "/railgun/v2/0-1-transact-response/json";
    const REPUBLISH_INTERVAL: Duration = Duration::from_secs(5);

    enum NodeEvent {
        Subscribed(usize, mpsc::Sender<WakuMessage>),
        Published(usize, Vec<u8>),
        ShutDown(usize),
        Dropped(usize),
    }

    /// The route of a harness's nodes, which a test drops and restores.
    struct FakeRoute {
        /// Connected peers. The route is ready while there are any.
        peers: watch::Sender<usize>,
        refreshes: AtomicUsize,
        /// Publications fail, after they are reported, while set.
        failing: AtomicBool,
    }

    impl FakeRoute {
        fn set_peers(&self, peers: usize) {
            self.peers.send_replace(peers);
        }

        fn refreshes(&self) -> usize {
            self.refreshes.load(Ordering::SeqCst)
        }
    }

    struct FakeNode {
        index: usize,
        route: Arc<FakeRoute>,
        /// Every publication after the first never completes.
        hang_republish: bool,
        publications: AtomicUsize,
        events: mpsc::UnboundedSender<NodeEvent>,
    }

    impl Drop for FakeNode {
        fn drop(&mut self) {
            let _ = self.events.send(NodeEvent::Dropped(self.index));
        }
    }

    #[async_trait::async_trait]
    impl SubmissionNode for FakeNode {
        fn local_peer_id(&self) -> Option<waku::PeerId> {
            None
        }

        fn pubsub_path(&self) -> &'static str {
            "/waku/2/rs/5/1"
        }

        fn route_health(&self) -> SubmissionRouteHealth {
            let peers = *self.route.peers.borrow();
            SubmissionRouteHealth {
                peers,
                lightpush: peers > 0,
                filter: peers > 0,
            }
        }

        fn refresh_route(&self) -> bool {
            self.route.refreshes.fetch_add(1, Ordering::SeqCst);
            true
        }

        async fn wait_ready(&self, bound: Duration) -> eyre::Result<()> {
            let mut peers = self.route.peers.subscribe();
            let connected = matches!(
                tokio::time::timeout(bound, peers.wait_for(|count| *count > 0)).await,
                Ok(Ok(_))
            );
            if connected {
                Ok(())
            } else {
                Err(eyre::eyre!("no peer connected"))
            }
        }

        async fn subscribe(
            &self,
            content_topics: Vec<String>,
        ) -> eyre::Result<mpsc::Receiver<WakuMessage>> {
            assert_eq!(content_topics, [RESPONSE_TOPIC]);
            let (responses_tx, responses) = mpsc::channel(4);
            let _ = self
                .events
                .send(NodeEvent::Subscribed(self.index, responses_tx));
            Ok(responses)
        }

        async fn publish(&self, content_topic: &str, payload: &[u8]) -> eyre::Result<()> {
            assert_eq!(content_topic, transact_topic(1));
            let _ = self
                .events
                .send(NodeEvent::Published(self.index, payload.to_vec()));
            if self.hang_republish && self.publications.fetch_add(1, Ordering::SeqCst) > 0 {
                std::future::pending::<()>().await;
            }
            if self.route.failing.load(Ordering::SeqCst) {
                return Err(eyre::eyre!("publish failed"));
            }
            Ok(())
        }

        async fn shutdown(&self, _bound: Duration) -> eyre::Result<()> {
            let _ = self.events.send(NodeEvent::ShutDown(self.index));
            Ok(())
        }
    }

    struct Harness {
        waku: PublicBroadcasterWaku,
        scope: SubmissionNodeScope,
        specs: Arc<Mutex<Vec<SubmissionNodeSpec>>>,
        events: mpsc::UnboundedReceiver<NodeEvent>,
        route: Arc<FakeRoute>,
        /// The route watch of every spawned submission.
        route_tx: SubmissionRouteSender,
    }

    /// Submissions seeded from `root`, with fake nodes numbered in build order.
    fn build_harness(
        root: Arc<WakuClient>,
        config: ClientConfig,
        http: HttpContext,
        ready: bool,
        hang_republish: bool,
    ) -> Harness {
        let scope = SubmissionNodeScope::new();
        let specs = Arc::new(Mutex::new(Vec::new()));
        let (events_tx, events) = mpsc::unbounded_channel();
        let route = Arc::new(FakeRoute {
            peers: watch::Sender::new(if ready { 3 } else { 0 }),
            refreshes: AtomicUsize::new(0),
            failing: AtomicBool::new(false),
        });
        let built = Arc::clone(&specs);
        let node_route = Arc::clone(&route);
        let waku = PublicBroadcasterWaku::new(root, config, http, scope.clone())
            .with_node_factory_for_tests(move |spec| {
                let mut built = built.lock().unwrap();
                built.push(spec);
                let node: Box<dyn SubmissionNode> = Box::new(FakeNode {
                    index: built.len() - 1,
                    route: Arc::clone(&node_route),
                    hang_republish,
                    publications: AtomicUsize::new(0),
                    events: events_tx.clone(),
                });
                Ok(node)
            });
        Harness {
            waku,
            scope,
            specs,
            events,
            route,
            route_tx: watch::Sender::new(SubmissionRouteProgress::default()),
        }
    }

    /// A root whose Waku is off: its snapshot is empty, and it can't subscribe or publish.
    fn disabled_root() -> Arc<WakuClient> {
        Arc::new(
            WakuClient::new_with_network(
                &ClientConfig::default(),
                RelayNetworkConfig::proxy(reqwest::Client::new()),
            )
            .unwrap(),
        )
    }

    /// A root that knows `count` configured peers, at a local port nothing listens on.
    fn seeded_root(count: usize) -> Arc<WakuClient> {
        let config = ClientConfig {
            direct_peers: (0..count)
                .map(|_| AdditionalPeer {
                    peer_id: libp2p::PeerId::random().to_string(),
                    addrs: vec!["/ip4/127.0.0.1/tcp/1".to_owned()],
                })
                .collect(),
            dns_enr_trees: Some(Vec::new()),
            ..ClientConfig::default()
        };
        Arc::new(WakuClient::new_with_network(&config, RelayNetworkConfig::direct()).unwrap())
    }

    /// Tor-mode networking whose every operation client is new, carries `x-route: submission`,
    /// and has its own Tor provider. Each is recorded in `opened`.
    fn tor_http(opened: Arc<Mutex<Vec<OperationHttpClient>>>) -> HttpContext {
        HttpContext::with_rpc_client_for_tests(reqwest::Client::new(), WalletNetworkMode::Tor)
            .with_operation_clients_for_tests(move || {
                let provider: WalletTorClientProvider = Arc::new(|| None);
                let operation = OperationHttpClient::for_tests(
                    tagged_reqwest_client("submission"),
                    OperationNetworkIsolation::Dedicated,
                )
                .with_tor_client_provider_for_tests(provider);
                opened.lock().unwrap().push(operation.clone());
                Ok(operation)
            })
    }

    async fn submit(
        waku: &PublicBroadcasterWaku,
        broadcaster: &PublicBroadcasterCandidate,
        timeout: Duration,
    ) -> eyre::Result<PublicBroadcasterResultKind> {
        submit_watched(waku, broadcaster, timeout, None).await
    }

    async fn submit_watched(
        waku: &PublicBroadcasterWaku,
        broadcaster: &PublicBroadcasterCandidate,
        timeout: Duration,
        route_tx: Option<SubmissionRouteSender>,
    ) -> eyre::Result<PublicBroadcasterResultKind> {
        submit_public_broadcaster_transaction(
            waku,
            TransactionRequest::default()
                .to(broadcaster.relay_adapt.expect("fixture relay adapt"))
                .input(Bytes::from_static(&[1, 2, 3]).into()),
            BTreeMap::default(),
            broadcaster,
            1_000_000_000,
            PublicBroadcasterDelivery {
                progress_tx: None,
                route_tx,
                timeout,
                republish_interval: REPUBLISH_INTERVAL,
            },
        )
        .await
    }

    /// A submission that reports its route on the harness's watch.
    fn spawn_submission(
        harness: &Harness,
        broadcaster: &PublicBroadcasterCandidate,
        timeout: Duration,
    ) -> tokio::task::JoinHandle<eyre::Result<PublicBroadcasterResultKind>> {
        let (waku, broadcaster) = (harness.waku.clone(), broadcaster.clone());
        let route_tx = harness.route_tx.clone();
        tokio::spawn(
            async move { submit_watched(&waku, &broadcaster, timeout, Some(route_tx)).await },
        )
    }

    async fn next_event(events: &mut mpsc::UnboundedReceiver<NodeEvent>) -> NodeEvent {
        events.recv().await.expect("node event")
    }

    /// The broadcaster's encrypted `txHash` answer to the transact request `payload`.
    fn tx_hash_response(viewing: &ViewingKeyData, payload: &[u8]) -> WakuMessage {
        let envelope: serde_json::Value = serde_json::from_slice(payload).unwrap();
        let pubkey: FixedBytes<32> =
            serde_json::from_value(envelope["params"]["pubkey"].clone()).unwrap();
        let encrypted_data =
            serde_json::from_value(envelope["params"]["encryptedData"].clone()).unwrap();
        let request =
            try_decrypt_transact_request(&viewing.viewing_private_key, pubkey.0, &encrypted_data)
                .unwrap()
                .unwrap();
        WakuMessage {
            content_topic: RESPONSE_TOPIC.to_owned(),
            payload: DecryptedTransactResponse::encrypted_tx_hash_message(
                None,
                &request.shared_key,
                TxHash::repeat_byte(3),
            )
            .unwrap(),
            ..WakuMessage::default()
        }
    }

    /// The root's Waku is off here, so a submission that subscribed or published on it would
    /// fail.
    #[tokio::test(start_paused = true)]
    async fn submissions_publish_on_their_own_nodes_and_tor_isolations() {
        let opened = Arc::default();
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::clone(&opened)),
            true,
            false,
        );
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);

        for _ in 0..2 {
            assert_eq!(
                submit(&harness.waku, &broadcaster, Duration::from_secs(1))
                    .await
                    .unwrap(),
                PublicBroadcasterResultKind::TimedOut
            );
        }

        {
            let opened = opened.lock().unwrap();
            let specs = harness.specs.lock().unwrap();
            assert_eq!((opened.len(), specs.len()), (2, 2));
            assert!(!Arc::ptr_eq(
                &opened[0].tor_client_provider().unwrap(),
                &opened[1].tor_client_provider().unwrap()
            ));
            for (spec, operation) in specs.iter().zip(opened.iter()) {
                assert_eq!(spec.network.mode, RelayNetworkMode::Tor);
                assert!(Arc::ptr_eq(
                    spec.network.tor_client.as_ref().unwrap(),
                    &operation.tor_client_provider().unwrap()
                ));
            }
        }
        let mut publishers = Vec::new();
        while let Ok(event) = harness.events.try_recv() {
            if let NodeEvent::Published(node, _) = event {
                publishers.push(node);
            }
        }
        assert_eq!(publishers, [0, 1]);
    }

    #[tokio::test]
    async fn submission_nodes_seed_from_the_root_snapshot_or_run_their_own_discovery() {
        let configured = ClientConfig {
            dns_enr_trees: Some(vec!["enrtree://configured@nodes.example.org".to_owned()]),
            max_peers: Some(40),
            ..ClientConfig::default()
        };

        // Three of the root's peers, and no discovery of the node's own.
        let root = seeded_root(5);
        let snapshot = root.seed_peers();
        assert_eq!(snapshot.len(), 5);
        let seeded = ClientConfig {
            backup_peers: snapshot[..1].to_vec(),
            ..configured.clone()
        };
        let harness = build_harness(root, seeded, tor_http(Arc::default()), true, false);
        drop(harness.waku.start_node().await.unwrap());
        let spec = harness.specs.lock().unwrap().pop().unwrap();
        let seeds = &spec.config.direct_peers;
        assert!(seeds.iter().all(|seed| snapshot.contains(seed)));
        let distinct: std::collections::HashSet<_> =
            seeds.iter().map(|seed| &seed.peer_id).collect();
        assert_eq!((seeds.len(), distinct.len()), (3, 3));
        assert_eq!(spec.config.dns_enr_trees, Some(Vec::new()));
        assert_eq!(spec.config.backup_peers, []);
        assert_eq!(spec.config.max_peers, Some(3));

        // No seed: the configured trees, resolved over the submission's operation client.
        let (url, recorded, server) =
            spawn_recording_rpc_mock(Arc::new(|_| serde_json::Value::Null)).await;
        let harness = build_harness(
            disabled_root(),
            configured.clone(),
            tor_http(Arc::default()),
            true,
            false,
        );
        drop(harness.waku.start_node().await.unwrap());
        let spec = harness.specs.lock().unwrap().pop().unwrap();
        assert_eq!(spec.config.direct_peers, []);
        assert_eq!(spec.config.dns_enr_trees, configured.dns_enr_trees);
        assert_eq!(spec.network.mode, RelayNetworkMode::Tor);
        spec.network
            .http_client
            .unwrap()
            .post(url)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(recorded.lock().unwrap()[0].0.as_deref(), Some("submission"));
        server.abort();

        // No seed and discovery disabled: no node, nothing published.
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig {
                dns_enr_trees: Some(Vec::new()),
                ..configured
            },
            tor_http(Arc::default()),
            true,
            false,
        );
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);
        let error = submit(&harness.waku, &broadcaster, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("DNS discovery is disabled"));
        assert!(harness.specs.lock().unwrap().is_empty());
        assert!(harness.events.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_submission_node_that_never_connects_fails_with_nothing_published() {
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::default()),
            false,
            false,
        );
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);

        let error = submit(&harness.waku, &broadcaster, Duration::from_mins(2))
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Waku delivery network is unavailable")
        );
        // Dropped without subscribing, publishing or a teardown delay.
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Dropped(0)
        ));
        assert!(harness.events.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_nwaku_publish_of_a_submission_node_uses_its_operation_client() {
        let (url, recorded, server) =
            spawn_recording_rpc_mock(Arc::new(|_| serde_json::Value::Null)).await;
        let nwaku_url = url.as_str().trim_end_matches('/').to_owned();
        let http = HttpContext::with_rpc_client_for_tests(
            reqwest::Client::new(),
            WalletNetworkMode::Direct,
        )
        .with_operation_clients_for_tests(|| {
            Ok(OperationHttpClient::for_tests(
                tagged_reqwest_client("submission"),
                OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
            ))
        });
        let harness = build_harness(
            seeded_root(1),
            ClientConfig {
                nwaku_url: Some(nwaku_url.clone()),
                ..ClientConfig::default()
            },
            http,
            true,
            false,
        );
        drop(harness.waku.start_node().await.unwrap());
        let spec = harness.specs.lock().unwrap().pop().unwrap();
        assert_eq!(spec.config.nwaku_url, Some(nwaku_url));

        // A node built from the spec. Its seed refuses connections, so only nwaku receives this.
        let node = WakuClient::new_with_network(&spec.config, spec.network).unwrap();
        node.publish(&transact_topic(1), b"{}").await.unwrap();

        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0.as_deref(), Some("submission"));
        server.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_republish_delays_no_result_and_the_node_ends_after_its_tail() {
        const TIMEOUT: Duration = Duration::from_secs(7);
        let (broadcaster, viewing) = sample_public_broadcaster_candidate(11);
        for respond in [true, false] {
            let mut harness = build_harness(
                disabled_root(),
                ClientConfig::default(),
                tor_http(Arc::default()),
                true,
                true,
            );
            let started = Instant::now();
            let submission = spawn_submission(&harness, &broadcaster, TIMEOUT);
            let NodeEvent::Subscribed(0, responses) = next_event(&mut harness.events).await else {
                panic!("the node subscribes before publishing");
            };
            let NodeEvent::Published(0, request) = next_event(&mut harness.events).await else {
                panic!("the node publishes the request");
            };
            // The first republish never completes.
            assert!(matches!(
                next_event(&mut harness.events).await,
                NodeEvent::Published(0, _)
            ));

            let expected = if respond {
                responses
                    .send(tx_hash_response(&viewing, &request))
                    .await
                    .unwrap();
                PublicBroadcasterResultKind::Submitted {
                    tx_hash: TxHash::repeat_byte(3).to_string(),
                }
            } else {
                PublicBroadcasterResultKind::TimedOut
            };
            assert_eq!(submission.await.unwrap().unwrap(), expected);
            let ended = Instant::now();
            let deadline = if respond { REPUBLISH_INTERVAL } else { TIMEOUT };
            assert!(ended - started < deadline + Duration::from_secs(1));

            assert!(matches!(
                next_event(&mut harness.events).await,
                NodeEvent::ShutDown(0)
            ));
            let tail = ended.elapsed();
            assert!(
                (Duration::from_secs(10)..=Duration::from_secs(60)).contains(&tail),
                "{tail:?}"
            );
            assert!(matches!(
                next_event(&mut harness.events).await,
                NodeEvent::Dropped(0)
            ));
        }
    }

    /// Stop and executor close drop the submission's future, and lock then closes the scope.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_submission_retires_its_node_through_the_tail_without_publishing() {
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);
        for close_scope in [false, true] {
            let mut harness = build_harness(
                disabled_root(),
                ClientConfig::default(),
                tor_http(Arc::default()),
                true,
                false,
            );
            let submission = spawn_submission(&harness, &broadcaster, Duration::from_mins(2));
            // Held open so that the submission is still waiting when it is aborted.
            let NodeEvent::Subscribed(0, _responses) = next_event(&mut harness.events).await else {
                panic!("the node subscribes before publishing");
            };
            assert!(matches!(
                next_event(&mut harness.events).await,
                NodeEvent::Published(0, _)
            ));

            submission.abort();
            assert!(submission.await.unwrap_err().is_cancelled());
            // Closed at once, while the node is still retiring.
            assert_eq!(
                harness.route_tx.borrow().state,
                SubmissionRouteState::Closed
            );
            let aborted = Instant::now();
            if close_scope {
                harness.scope.close();
            }

            // Any republish would arrive before the shutdown.
            assert!(matches!(
                next_event(&mut harness.events).await,
                NodeEvent::ShutDown(0)
            ));
            let tail = aborted.elapsed();
            let expected = if close_scope {
                Duration::ZERO..=Duration::from_secs(1)
            } else {
                Duration::from_secs(10)..=Duration::from_secs(60)
            };
            assert!(expected.contains(&tail), "{tail:?}");
            assert!(matches!(
                next_event(&mut harness.events).await,
                NodeEvent::Dropped(0)
            ));
            assert!(harness.events.try_recv().is_err());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn closing_the_scope_ends_a_tail_at_once_and_refuses_new_nodes() {
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::default()),
            true,
            false,
        );
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);

        // Publications at 0, 5 and 10 seconds, then the timeout.
        let result = spawn_submission(&harness, &broadcaster, Duration::from_secs(12)).await;
        assert_eq!(
            result.unwrap().unwrap(),
            PublicBroadcasterResultKind::TimedOut
        );
        let mut publications = 0;
        while let Ok(event) = harness.events.try_recv() {
            publications += usize::from(matches!(event, NodeEvent::Published(0, _)));
        }
        assert_eq!(publications, 3);

        // Shorter than any tail, and long enough for two more republishes.
        tokio::time::sleep(Duration::from_secs(9)).await;
        assert!(harness.events.try_recv().is_err());

        let closed = Instant::now();
        harness.scope.close();
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::ShutDown(0)
        ));
        assert!(closed.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Dropped(0)
        ));

        // A callback that still holds the closed scope gets no node.
        let error = submit(&harness.waku, &broadcaster, Duration::from_secs(12))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("locked"));
        assert_eq!(harness.specs.lock().unwrap().len(), 1);
        assert!(harness.events.try_recv().is_err());
    }

    fn route_progress(
        state: SubmissionRouteState,
        peers: usize,
        recoveries_used: u32,
        publishes: u32,
    ) -> SubmissionRouteProgress {
        SubmissionRouteProgress {
            state,
            peers,
            recoveries_used,
            publishes,
        }
    }

    /// The node keeps its identity: no second node is built, and node 0 publishes throughout.
    #[tokio::test(start_paused = true)]
    async fn a_lost_route_is_recovered_on_new_circuits_and_its_response_accepted() {
        let opened = Arc::default();
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::clone(&opened)),
            true,
            false,
        );
        let (broadcaster, viewing) = sample_public_broadcaster_candidate(11);
        let started = Instant::now();
        let at = |millis| started + Duration::from_millis(millis);
        let submission = spawn_submission(&harness, &broadcaster, Duration::from_mins(2));
        let NodeEvent::Subscribed(0, responses) = next_event(&mut harness.events).await else {
            panic!("the node subscribes before publishing");
        };
        let NodeEvent::Published(0, request) = next_event(&mut harness.events).await else {
            panic!("the node publishes the request");
        };
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Ready, 3, 0, 1)
        );

        // Lost from the first poll, at 1 s, so the grace ends at 11 s.
        harness.route.set_peers(0);
        tokio::time::sleep_until(at(10_500)).await;
        assert_eq!(harness.route.refreshes(), 0);
        tokio::time::sleep_until(at(11_500)).await;
        let reisolations = opened.lock().unwrap()[0].reisolations_for_tests();
        assert_eq!((reisolations, harness.route.refreshes()), (1, 1));
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Reconnecting, 0, 1, 1)
        );
        // Nothing went out while the route was down, though a republish was due at 5 s.
        assert!(harness.events.try_recv().is_err());

        harness.route.set_peers(2);
        let NodeEvent::Published(0, republished) = next_event(&mut harness.events).await else {
            panic!("the recovered node republishes");
        };
        assert_eq!(republished, request);
        assert!((at(11_500)..at(12_000)).contains(&Instant::now()));
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Ready, 2, 1, 2)
        );

        responses
            .send(tx_hash_response(&viewing, &request))
            .await
            .unwrap();
        assert_eq!(
            submission.await.unwrap().unwrap(),
            PublicBroadcasterResultKind::Submitted {
                tx_hash: TxHash::repeat_byte(3).to_string(),
            }
        );
        // Closed with its counts kept.
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Closed, 2, 1, 2)
        );
        assert_eq!(harness.specs.lock().unwrap().len(), 1);
    }

    /// A republish left pending by the route's loss doesn't hold off the recovered route's
    /// resend.
    #[tokio::test(start_paused = true)]
    async fn a_recovered_route_resends_at_once_past_a_pending_publish() {
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::default()),
            true,
            true,
        );
        let (broadcaster, viewing) = sample_public_broadcaster_candidate(11);
        let started = Instant::now();
        let at = |millis| started + Duration::from_millis(millis);
        let submission = spawn_submission(&harness, &broadcaster, Duration::from_mins(2));
        let NodeEvent::Subscribed(0, responses) = next_event(&mut harness.events).await else {
            panic!("the node subscribes before publishing");
        };
        let NodeEvent::Published(0, request) = next_event(&mut harness.events).await else {
            panic!("the node publishes the request");
        };
        // The republish at 5 s never completes. The route is lost from the poll at 6 s, so
        // recovery starts at 16 s.
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));
        harness.route.set_peers(0);
        tokio::time::sleep_until(at(16_500)).await;
        assert_eq!(harness.route.refreshes(), 1);

        harness.route.set_peers(2);
        let NodeEvent::Published(0, resent) = next_event(&mut harness.events).await else {
            panic!("the recovered node resends");
        };
        assert_eq!(resent, request);
        assert!((at(16_500)..at(17_500)).contains(&Instant::now()));
        assert_eq!(harness.route_tx.borrow().publishes, 3);

        responses
            .send(tx_hash_response(&viewing, &request))
            .await
            .unwrap();
        assert_eq!(
            submission.await.unwrap().unwrap(),
            PublicBroadcasterResultKind::Submitted {
                tx_hash: TxHash::repeat_byte(3).to_string(),
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_short_drop_recovers_nothing_and_publishes_count_only_on_a_ready_route() {
        let opened = Arc::default();
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::clone(&opened)),
            true,
            false,
        );
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);
        let started = Instant::now();
        let at = |millis| started + Duration::from_millis(millis);
        let submission = spawn_submission(&harness, &broadcaster, Duration::from_mins(2));
        let NodeEvent::Subscribed(0, _responses) = next_event(&mut harness.events).await else {
            panic!("the node subscribes before publishing");
        };
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));

        // Down for the polls at 1 to 10 s, 9 s apart: short of the grace.
        harness.route.set_peers(0);
        tokio::time::sleep_until(at(10_500)).await;
        assert_eq!(harness.route_tx.borrow().publishes, 1);
        harness.route.set_peers(3);
        // The republish due at 5 s goes out at the first ready poll, and the next one fails.
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));
        assert_eq!(Instant::now(), at(11_000));
        harness.route.failing.store(true, Ordering::SeqCst);
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));
        assert_eq!(Instant::now(), at(16_000));
        tokio::time::sleep_until(at(16_500)).await;

        assert_eq!(harness.route_tx.borrow().publishes, 3);
        let reisolations = opened.lock().unwrap()[0].reisolations_for_tests();
        assert_eq!((reisolations, harness.route.refreshes()), (0, 0));
        submission.abort();
    }

    /// A failed publish may still have reached peers, so even the first one waits for a response.
    #[tokio::test(start_paused = true)]
    async fn a_failed_initial_publish_still_waits_for_the_response() {
        const TIMEOUT: Duration = Duration::from_secs(7);
        let (broadcaster, viewing) = sample_public_broadcaster_candidate(11);
        for respond in [true, false] {
            let mut harness = build_harness(
                disabled_root(),
                ClientConfig::default(),
                tor_http(Arc::default()),
                true,
                false,
            );
            harness.route.failing.store(true, Ordering::SeqCst);
            let submission = spawn_submission(&harness, &broadcaster, TIMEOUT);
            let NodeEvent::Subscribed(0, responses) = next_event(&mut harness.events).await else {
                panic!("the node subscribes before publishing");
            };
            let NodeEvent::Published(0, request) = next_event(&mut harness.events).await else {
                panic!("the node publishes the request");
            };

            let expected = if respond {
                responses
                    .send(tx_hash_response(&viewing, &request))
                    .await
                    .unwrap();
                PublicBroadcasterResultKind::Submitted {
                    tx_hash: TxHash::repeat_byte(3).to_string(),
                }
            } else {
                PublicBroadcasterResultKind::TimedOut
            };
            assert_eq!(submission.await.unwrap().unwrap(), expected);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn route_recovery_stops_at_its_limit_and_the_submission_times_out_on_time() {
        const TIMEOUT: Duration = Duration::from_secs(100);
        let opened = Arc::default();
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::clone(&opened)),
            true,
            false,
        );
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);
        let started = Instant::now();
        let at = |millis| started + Duration::from_millis(millis);
        let submission = spawn_submission(&harness, &broadcaster, TIMEOUT);
        let NodeEvent::Subscribed(0, _responses) = next_event(&mut harness.events).await else {
            panic!("the node subscribes before publishing");
        };
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));

        // The first recovery starts at 11 s and waits out its 30 s; the second follows at once.
        harness.route.set_peers(0);
        tokio::time::sleep_until(at(40_500)).await;
        assert_eq!(harness.route.refreshes(), 1);
        tokio::time::sleep_until(at(41_500)).await;
        assert_eq!(harness.route.refreshes(), 2);
        harness.route.set_peers(3);
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));

        // Lost again, with no recovery left.
        harness.route.set_peers(0);
        tokio::time::sleep_until(at(99_500)).await;
        let reisolations = opened.lock().unwrap()[0].reisolations_for_tests();
        assert_eq!((reisolations, harness.route.refreshes()), (2, 2));
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Reconnecting, 0, 2, 2)
        );
        assert_eq!(
            submission.await.unwrap().unwrap(),
            PublicBroadcasterResultKind::TimedOut
        );
        assert!((at(100_000)..at(100_500)).contains(&Instant::now()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_direct_mode_route_reconnects_on_its_own_without_recovery() {
        let opened: Arc<Mutex<Vec<OperationHttpClient>>> = Arc::default();
        let recorded = Arc::clone(&opened);
        let http = HttpContext::with_rpc_client_for_tests(
            reqwest::Client::new(),
            WalletNetworkMode::Direct,
        )
        .with_operation_clients_for_tests(move || {
            let operation = OperationHttpClient::for_tests(
                reqwest::Client::new(),
                OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
            );
            recorded.lock().unwrap().push(operation.clone());
            Ok(operation)
        });
        let mut harness =
            build_harness(disabled_root(), ClientConfig::default(), http, true, false);
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);
        let started = Instant::now();
        let submission = spawn_submission(&harness, &broadcaster, Duration::from_mins(2));
        let NodeEvent::Subscribed(0, _responses) = next_event(&mut harness.events).await else {
            panic!("the node subscribes before publishing");
        };
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));

        // Well past the grace.
        harness.route.set_peers(0);
        tokio::time::sleep_until(started + Duration::from_millis(20_500)).await;
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Reconnecting, 0, 0, 1)
        );
        let reisolations = opened.lock().unwrap()[0].reisolations_for_tests();
        assert_eq!((reisolations, harness.route.refreshes()), (0, 0));

        // The next poll finds the node ready and sends the overdue republish.
        harness.route.set_peers(2);
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Ready, 2, 0, 2)
        );
        submission.abort();
    }
}

mod estimates_and_gas;
mod fee_handling;
mod fee_policy;
mod payloads;
mod selection;
