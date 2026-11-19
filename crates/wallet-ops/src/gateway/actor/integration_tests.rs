use super::super::GatewayHandle;
use super::*;
use crate::AppSettingsDbExt as _;
use futures_util::{SinkExt, StreamExt};
use local_db::DbConfig;
use std::net::{Ipv4Addr, Ipv6Addr};

async fn available_port() -> std::num::NonZeroU16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    std::num::NonZeroU16::new(listener.local_addr().unwrap().port()).unwrap()
}

async fn socket(address: std::net::SocketAddr) -> Socket {
    let stream = TcpStream::connect(address).await.unwrap();
    tokio_tungstenite::client_async(format!("ws://{address}"), stream)
        .await
        .unwrap()
        .0
}

async fn read(socket: &mut Socket) -> Vec<u8> {
    match tokio::time::timeout(Duration::from_secs(30), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
    {
        Message::Binary(bytes) => bytes.to_vec(),
        _ => panic!("expected binary protocol frame"),
    }
}

async fn pair(socket: &mut Socket, code: PairingCode) -> (Connection, PeerId, SessionSecret) {
    let (mut protocol, hello) = Connection::client_pair(code);
    socket.send(Message::Binary(hello.into())).await.unwrap();
    let mut credential = None;
    loop {
        let frame = read(socket).await;
        let step = protocol.receive_handshake(&frame).unwrap();
        if let Some(output) = step.outbound {
            socket.send(Message::Binary(output.into())).await.unwrap();
        }
        if step.event == Some(HandshakeEvent::CredentialReceived) {
            credential = Some(SessionSecret::from_storage(
                protocol.pending_credential().unwrap().export_for_storage(),
            ));
            let ack = protocol.acknowledge_credential().unwrap();
            socket.send(Message::Binary(ack.into())).await.unwrap();
        }
        if step.event == Some(HandshakeEvent::Authenticated) {
            let peer = protocol.authenticated_peer_id().unwrap();
            return (protocol, peer, credential.unwrap());
        }
    }
}

async fn next_message(socket: &mut Socket, protocol: &mut Connection) -> serde_json::Value {
    let bytes = read(socket).await;
    let message = protocol.receive_frame(&bytes, 0).unwrap().unwrap();
    serde_json::from_slice(&message).unwrap()
}

/// The next message other than a heartbeat. The gateway sends one after 20s without other
/// output, so in a slow run it can arrive between any two expected messages.
async fn state(socket: &mut Socket, protocol: &mut Connection) -> serde_json::Value {
    // One bound for the whole wait: heartbeats keep arriving, so the per-read bound alone
    // would never expire while an expected message is missing.
    tokio::time::timeout(Duration::from_secs(30), async move {
        loop {
            let message = next_message(socket, protocol).await;
            if message["type"] != "heartbeat" {
                return message;
            }
        }
    })
    .await
    .expect("no message other than heartbeats within 30s")
}

async fn idle_ui_snapshot(
    socket: &mut Socket,
    protocol: &mut Connection,
    generation: u64,
    locked: bool,
) {
    let mut snapshot = state(socket, protocol).await;
    // Unlocked transport fixtures can already have networks and saved grants.
    // Provider tests cover their contents; this helper verifies idle/redacted UI.
    if locked {
        assert_eq!(snapshot["chains"], serde_json::json!([]));
        assert_eq!(snapshot["permissions"], serde_json::json!([]));
    }
    snapshot.as_object_mut().unwrap().remove("chains");
    snapshot.as_object_mut().unwrap().remove("permissions");
    assert_eq!(
        snapshot
            .as_object_mut()
            .unwrap()
            .remove("chain_management_supported"),
        (!locked).then_some(serde_json::Value::Bool(true))
    );
    assert_eq!(
        snapshot,
        serde_json::json!({
            "type": "ui_snapshot",
            "version": 1,
            "generation": generation,
            "locked": locked,
            "native_usd_pricing": {},
            "accounts": [],
            "private_view_supported": false,
            "private_view": null,
            "public_view": {
                "selected_account": null,
                "selected_chain": null,
                "balances": [],
                "drafts": [],
                "refreshing": false,
                "balance_error": false,
                "mimic_railway_shields_by_default": false,
            },
            "ui_error": null,
            "pending_connects": [],
            "pending_requests": [],
        })
    );
}

async fn application(socket: &mut Socket, protocol: &mut Connection, message: serde_json::Value) {
    for frame in protocol
        .seal_message(&serde_json::to_vec(&message).unwrap())
        .unwrap()
    {
        socket.send(Message::Binary(frame.into())).await.unwrap();
    }
}

#[tokio::test]
async fn paired_unlock_permission_is_enforced_and_revocation_invalidates_native_work() {
    async fn unlock_state(socket: &mut Socket, protocol: &mut Connection) -> serde_json::Value {
        loop {
            let value = state(socket, protocol).await;
            if value["type"] == "unlock_state" {
                return value;
            }
        }
    }
    // Fail instead of hanging if an expected frame or event never arrives.
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        let root =
            std::env::temp_dir().join(format!("gateway-unlock-{}", alloy::hex::encode(random)));
        let db = Arc::new(
            DbStore::open(DbConfig {
                root_dir: root.clone(),
            })
            .unwrap(),
        );
        let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
        handle
            .configure(GatewayConfig {
                enabled: true,
                bind_address: Ipv4Addr::LOCALHOST.into(),
                port: available_port().await,
            })
            .await
            .unwrap();
        let code = handle.issue_pairing_code_with_unlock(true).await.unwrap();
        // Copy the address out first: holding the watch guard across the await deadlocks
        // this single-threaded runtime when the actor publishes a snapshot meanwhile.
        let address = handle.snapshots().borrow().listener_addr.unwrap();
        let mut socket = socket(address).await;
        let (mut protocol, peer, _) = pair(&mut socket, code.code).await;
        state(&mut socket, &mut protocol).await;
        assert!(handle.snapshots().borrow().peers[0].allow_unlock);
        let mut events = handle.ui_events();
        handle.set_peer_allow_unlock(peer, false).await.unwrap();
        application(&mut socket, &mut protocol, serde_json::json!({"type":"unlock", "version":1, "generation":1, "attempt_id":"denied", "command":{"action":"password", "password":"synthetic password"}})).await;
        assert_eq!(
            unlock_state(&mut socket, &mut protocol).await["view"]["allowed"],
            false
        );
        assert!(
            events.try_recv().is_err(),
            "unpermitted input never reaches the vault owner"
        );
        handle.set_peer_allow_unlock(peer, true).await.unwrap();
        assert!(Registry::load(&db).unwrap().peers[0].allow_unlock);
        application(&mut socket, &mut protocol, serde_json::json!({"type":"unlock", "version":1, "generation":1, "attempt_id":"accepted", "command":{"action":"password", "password":"synthetic password"}})).await;
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        let super::super::GatewayUiEventKind::Unlock { request } = event.kind else {
            panic!("unlock request");
        };
        let (_, guard) = request.take().unwrap();
        guard.finish(super::super::GatewayUnlockPhase::Passphrase);
        handle.set_peer_allow_unlock(peer, false).await.unwrap();
        assert!(!guard.is_current());
        guard.finish(super::super::GatewayUnlockPhase::Complete);
        assert!(
            !guard.is_current(),
            "revoked work cannot regain installation authority"
        );
        drop(guard);
        handle.shutdown().await.unwrap();
        drop(socket);
        drop(handle);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .expect("paired unlock scenario timed out");
}

async fn pairing_attempt(address: std::net::SocketAddr, admitted: bool) {
    let mut connection = socket(address).await;
    connection
        .send(Message::Binary(ClientHello::pair().encode().into()))
        .await
        .unwrap();
    if admitted {
        // Abort after the first server flight, before proving knowledge of the code.
        read(&mut connection).await;
    } else {
        let rejected = tokio::time::timeout(Duration::from_secs(5), connection.next())
            .await
            .unwrap();
        assert!(!matches!(rejected, Some(Ok(Message::Binary(_)))));
    }
}

#[tokio::test]
async fn aborted_pairing_attempts_exhaust_codes_and_survive_listener_rebind() {
    let mut random = [0; 16];
    getrandom::fill(&mut random).unwrap();
    let root = std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root.clone(),
        })
        .unwrap(),
    );
    // Finish before the listener's first ten-second token refill.
    tokio::time::timeout(Duration::from_secs(8), async {
        let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
        let enabled = GatewayConfig {
            enabled: true,
            bind_address: Ipv4Addr::LOCALHOST.into(),
            port: available_port().await,
        };
        handle.configure(enabled).await.unwrap();
        let address = handle.snapshots().borrow().listener_addr.unwrap();
        handle.issue_pairing_code().await.unwrap();
        for _ in 0..3 {
            pairing_attempt(address, true).await;
        }
        // Rejection for the exhausted code spends the fourth listener token.
        pairing_attempt(address, false).await;

        handle.issue_pairing_code().await.unwrap();
        for _ in 0..2 {
            pairing_attempt(address, true).await;
        }
        pairing_attempt(address, false).await;
        handle.issue_pairing_code().await.unwrap();
        pairing_attempt(address, false).await;

        handle
            .configure(GatewayConfig {
                enabled: false,
                ..enabled
            })
            .await
            .unwrap();
        assert!(handle.snapshots().borrow().listener_addr.is_none());
        handle.configure(enabled).await.unwrap();
        let address = handle.snapshots().borrow().listener_addr.unwrap();
        handle.issue_pairing_code().await.unwrap();
        pairing_attempt(address, false).await;
        handle.shutdown().await.unwrap();
    })
    .await
    .expect("pairing admission scenario must finish before token refill");
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

/// Sends one plain HTTP request on the gateway listener and reads the whole response.
async fn install_request(address: std::net::SocketAddr, target: &str) -> (String, Vec<u8>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = TcpStream::connect(address).await.unwrap();
    let request = format!("GET {target} HTTP/1.1\r\nHost: {address}\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    (
        String::from_utf8(response[..split].to_vec()).unwrap(),
        response[split + 4..].to_vec(),
    )
}

#[tokio::test]
async fn install_page_and_bundle_are_served_beside_the_transport() {
    let mut random = [0; 16];
    getrandom::fill(&mut random).unwrap();
    let root = std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root.clone(),
        })
        .unwrap(),
    );
    let bundle = GatewayInstallBundle {
        zip: b"PK-test",
        version: "9.9.9",
    };
    let handle = GatewayHandle::start(db.clone(), true, 1, bundle);
    handle
        .configure(GatewayConfig {
            enabled: true,
            bind_address: Ipv4Addr::LOCALHOST.into(),
            port: available_port().await,
        })
        .await
        .unwrap();
    let address = handle.snapshots().borrow().listener_addr.unwrap();

    let (head, body) = install_request(address, "/install").await;
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert!(
        head.contains("Content-Type: text/html; charset=utf-8"),
        "{head}"
    );
    assert!(String::from_utf8(body).unwrap().contains("9.9.9"));

    let (head, body) = install_request(address, "/install/RailOxide-Extension.zip").await;
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert_eq!(body, b"PK-test");

    // The same listener keeps upgrading protocol clients.
    let mut connection = socket(address).await;
    connection.close(None).await.unwrap();

    handle.shutdown().await.unwrap();
    drop(handle);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn reported_extension_versions_reach_the_snapshot_and_ignore_invalid_reports() {
    let mut random = [0; 16];
    getrandom::fill(&mut random).unwrap();
    let root = std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root.clone(),
        })
        .unwrap(),
    );
    let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
    handle
        .configure(GatewayConfig {
            enabled: true,
            bind_address: Ipv4Addr::LOCALHOST.into(),
            port: available_port().await,
        })
        .await
        .unwrap();
    let address = handle.snapshots().borrow().listener_addr.unwrap();
    let offer = handle.issue_pairing_code().await.unwrap();
    let mut connection = socket(address).await;
    let (mut protocol, peer, _secret) = pair(&mut connection, offer.code).await;
    state(&mut connection, &mut protocol).await;
    idle_ui_snapshot(&mut connection, &mut protocol, 1, true).await;

    application(
        &mut connection,
        &mut protocol,
        serde_json::json!({"type":"client_info","version":1,"extension_version":"0.0.9"}),
    )
    .await;
    let mut snapshots = handle.snapshots();
    tokio::time::timeout(
        Duration::from_secs(5),
        snapshots.wait_for(|snapshot| {
            snapshot.peers.first().is_some_and(|reported| {
                reported.id == peer && reported.extension_version.as_deref() == Some("0.0.9")
            })
        }),
    )
    .await
    .unwrap()
    .unwrap();

    // An unusable report is dropped: it neither replaces the known version nor retires
    // the session, so the next command is still answered.
    application(
        &mut connection,
        &mut protocol,
        serde_json::json!({"type":"client_info","version":1,"extension_version":"abc"}),
    )
    .await;
    application(
        &mut connection,
        &mut protocol,
        serde_json::json!({"type":"heartbeat","version":1}),
    )
    .await;
    assert_eq!(
        next_message(&mut connection, &mut protocol).await["type"],
        "heartbeat"
    );
    assert_eq!(
        snapshots.borrow().peers[0].extension_version.as_deref(),
        Some("0.0.9")
    );

    handle.shutdown().await.unwrap();
    drop(connection);
    drop(handle);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn oversized_fragmented_upgrade_still_handshakes() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut random = [0; 16];
    getrandom::fill(&mut random).unwrap();
    let root = std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root.clone(),
        })
        .unwrap(),
    );
    let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
    handle
        .configure(GatewayConfig {
            enabled: true,
            bind_address: Ipv4Addr::LOCALHOST.into(),
            port: available_port().await,
        })
        .await
        .unwrap();
    let address = handle.snapshots().borrow().listener_addr.unwrap();

    // A browser can carry kilobytes of cookies, arriving in pieces; the classifier has to wait
    // for the whole head and still hand a request this size to the handshake.
    let request = format!(
        "GET / HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Cookie: {}\r\n\r\n",
        "a".repeat(9 * 1024)
    );
    let request = request.as_bytes();
    let mut stream = TcpStream::connect(address).await.unwrap();
    let split = request.len() / 2;
    stream.write_all(&request[..split]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    stream.write_all(&request[split..]).await.unwrap();

    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut byte = [0; 1];
        while !response.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            response.extend_from_slice(&byte);
        }
    })
    .await
    .expect("the upgrade response must arrive");
    let head = String::from_utf8(response).unwrap();
    assert_eq!(
        head.lines().next(),
        Some("HTTP/1.1 101 Switching Protocols"),
        "{head}"
    );

    drop(stream);
    handle.shutdown().await.unwrap();
    drop(handle);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn ipv6_listener_binds_and_persists_selected_address() {
    let mut random = [0; 16];
    getrandom::fill(&mut random).unwrap();
    let root = std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root.clone(),
        })
        .unwrap(),
    );
    let reserved = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
    let address = reserved.local_addr().unwrap();
    let config = GatewayConfig {
        enabled: true,
        bind_address: address.ip(),
        port: std::num::NonZeroU16::new(address.port()).unwrap(),
    };
    drop(reserved);
    let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
    handle.configure(config).await.unwrap();
    assert_eq!(handle.snapshots().borrow().listener_addr, Some(address));
    let mut connection = socket(address).await;
    connection.close(None).await.unwrap();
    assert_eq!(Registry::load(&db).unwrap().config, config);
    handle.shutdown().await.unwrap();
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn authenticated_peer_activity_is_batched_between_registry_writes() {
    let mut random = [0; 16];
    getrandom::fill(&mut random).unwrap();
    let root = std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root.clone(),
        })
        .unwrap(),
    );
    let mut registry = Registry {
        config: GatewayConfig {
            enabled: true,
            port: available_port().await,
            ..GatewayConfig::default()
        },
        ..Registry::default()
    };
    let initial_timestamp = 1;
    for id in [1, 2] {
        registry.peers.push(Peer {
            id: [id; 16],
            secret: StoredSecret([id; 32]),
            label: None,
            allow_unlock: false,
            paired_at: initial_timestamp,
            last_active_at: initial_timestamp,
        });
    }
    registry.save(&db).unwrap();
    let started = Instant::now();
    let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
    let mut snapshots = handle.snapshots();
    snapshots
        .wait_for(|snapshot| snapshot.listener_addr.is_some())
        .await
        .unwrap();
    let address = snapshots.borrow().listener_addr.unwrap();
    let mut connections = Vec::new();
    for peer in &registry.peers {
        let mut connection = socket(address).await;
        let (mut protocol, hello) = Connection::client_reconnect(
            PeerId::from_bytes(peer.id),
            SessionSecret::from_storage(peer.secret.0),
        );
        connection
            .send(Message::Binary(hello.into()))
            .await
            .unwrap();
        loop {
            let step = protocol
                .receive_handshake(&read(&mut connection).await)
                .unwrap();
            if let Some(output) = step.outbound {
                connection
                    .send(Message::Binary(output.into()))
                    .await
                    .unwrap();
            }
            if step.event == Some(HandshakeEvent::Authenticated) {
                break;
            }
        }
        state(&mut connection, &mut protocol).await;
        idle_ui_snapshot(&mut connection, &mut protocol, 1, true).await;
        application(
            &mut connection,
            &mut protocol,
            serde_json::json!({"type":"heartbeat","version":1}),
        )
        .await;
        assert_eq!(
            next_message(&mut connection, &mut protocol).await["type"],
            "heartbeat"
        );
        connections.push(connection);
    }
    snapshots
        .wait_for(|snapshot| {
            snapshot
                .peers
                .iter()
                .all(|peer| peer.last_active_at > initial_timestamp)
        })
        .await
        .unwrap();
    let active: Vec<_> = snapshots
        .borrow()
        .peers
        .iter()
        .map(|peer| peer.last_active_at)
        .collect();
    assert!(
        Registry::load(&db)
            .unwrap()
            .peers
            .iter()
            .all(|peer| peer.last_active_at == initial_timestamp)
    );
    // Complete real socket I/O before pausing; bound the deadline on both sides of setup.
    let authenticated = Instant::now();
    tokio::time::pause();
    let before_flush = started + Duration::from_mins(4);
    tokio::time::advance(before_flush.saturating_duration_since(Instant::now())).await;
    assert!(
        Registry::load(&db)
            .unwrap()
            .peers
            .iter()
            .all(|peer| peer.last_active_at == initial_timestamp)
    );
    let after_flush = authenticated + ACTIVITY_FLUSH_INTERVAL;
    tokio::time::advance(after_flush.saturating_duration_since(Instant::now())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let persisted = Registry::load(&db).unwrap();
    assert_eq!(
        persisted
            .peers
            .iter()
            .map(|peer| peer.last_active_at)
            .collect::<Vec<_>>(),
        active
    );
    assert!(
        persisted
            .peers
            .iter()
            .all(|peer| peer.paired_at == initial_timestamp)
    );
    handle.shutdown().await.unwrap();
    drop(connections);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn pairing_lock_revocation_and_registry_restart() {
    let mut random = [0; 16];
    getrandom::fill(&mut random).unwrap();
    let root = std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root.clone(),
        })
        .unwrap(),
    );
    let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let occupied_port = occupied.local_addr().unwrap().port();
    let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
    let conflicting = GatewayConfig {
        enabled: true,
        bind_address: Ipv4Addr::LOCALHOST.into(),
        port: std::num::NonZeroU16::new(occupied_port).unwrap(),
    };
    assert_eq!(
        handle.configure(conflicting).await,
        Err(GatewayError::PortInUse(occupied_port))
    );
    assert!(handle.snapshots().borrow().listener_addr.is_none());
    assert_eq!(
        handle.snapshots().borrow().error,
        Some(GatewayError::PortInUse(occupied_port))
    );
    assert_eq!(Registry::load(&db).unwrap().config, conflicting);
    let config = GatewayConfig {
        port: available_port().await,
        ..conflicting
    };
    handle.configure(config).await.unwrap();
    let mut snapshots = handle.snapshots();
    snapshots
        .wait_for(|snapshot| snapshot.listener_addr.is_some())
        .await
        .unwrap();
    let address = snapshots.borrow().listener_addr.unwrap();
    assert_eq!(
        address,
        SocketAddr::new(config.bind_address, config.port.get())
    );
    assert!(snapshots.borrow().error.is_none());
    let mut unauthenticated = socket(address).await;
    unauthenticated
        .send(Message::Binary(
            br#"{"type":"get_state","version":1}"#.to_vec().into(),
        ))
        .await
        .unwrap();
    let rejected = tokio::time::timeout(Duration::from_secs(5), unauthenticated.next())
        .await
        .unwrap();
    assert!(!matches!(rejected, Some(Ok(Message::Binary(_)))));
    let offer = handle.issue_pairing_code().await.unwrap();
    let mut connection = socket(address).await;
    let pairing_started = epoch_secs();
    let (mut protocol, peer, secret) = pair(&mut connection, offer.code).await;
    let persisted = Registry::load(&db).unwrap();
    let paired_at = persisted.peers[0].paired_at;
    assert!((pairing_started..=epoch_secs()).contains(&paired_at));
    assert_eq!(persisted.peers[0].last_active_at, paired_at);
    let first = state(&mut connection, &mut protocol).await;
    assert_eq!(first["locked"], true);
    assert_eq!(first["generation"], 1);
    idle_ui_snapshot(&mut connection, &mut protocol, 1, true).await;
    // Successful pairing consumes the code even with reservations remaining.
    pairing_attempt(address, false).await;
    let store = crate::vault::DesktopVaultStore::from_db(Arc::clone(&db));
    let view = super::super::provider::tests::initialize(&store);
    handle
        .update_wallet_state(super::super::provider::tests::state(&view), 2)
        .await
        .unwrap();
    assert_eq!(state(&mut connection, &mut protocol).await["locked"], false);
    idle_ui_snapshot(&mut connection, &mut protocol, 2, false).await;
    handle.update_desktop_state(true, 3).await.unwrap();
    let locked = state(&mut connection, &mut protocol).await;
    assert_eq!(locked["locked"], true);
    assert_eq!(locked["generation"], 3);
    idle_ui_snapshot(&mut connection, &mut protocol, 3, true).await;
    // The authenticated transport owns the peer identity and document registration.
    handle
        .update_wallet_state(super::super::provider::tests::state(&view), 4)
        .await
        .unwrap();
    assert_eq!(state(&mut connection, &mut protocol).await["generation"], 4);
    idle_ui_snapshot(&mut connection, &mut protocol, 4, false).await;
    application(&mut connection, &mut protocol, serde_json::json!({"type":"register_document","version":1,"document":"invalid","url":"not a URL"})).await;
    let invalid = state(&mut connection, &mut protocol).await;
    assert_eq!(invalid["error"]["code"], -32602);
    assert!(!invalid.to_string().contains("not a URL"));
    application(&mut connection, &mut protocol, serde_json::json!({"type":"register_document","version":1,"document":"page","url":"https://example.invalid/path?q=one#fragment"})).await;
    assert_eq!(
        state(&mut connection, &mut protocol).await["accounts"],
        serde_json::json!([])
    );
    application(&mut connection, &mut protocol, serde_json::json!({"type":"provider_request","version":1,"document":"page","request_id":"connect","method":"eth_requestAccounts","params":[]})).await;
    let prompt = state(&mut connection, &mut protocol).await;
    let pending = &prompt["pending_connects"][0];
    assert_eq!(pending["url"], "https://example.invalid/");
    assert_eq!(
        pending["paired_peer_id"],
        alloy::hex::encode(peer.to_bytes())
    );
    let address = pending["accounts"][0]["address"].clone();
    application(&mut connection, &mut protocol, serde_json::json!({"type":"resolve_connect","version":1,"request_id":pending["request_id"],"public_account_uuid":pending["accounts"][0]["uuid"],"chain_id":1})).await;
    let granted_state = state(&mut connection, &mut protocol).await;
    assert_eq!(granted_state["type"], "provider_state");
    assert_eq!(granted_state["accounts"], serde_json::json!([address]));
    assert_eq!(
        state(&mut connection, &mut protocol).await["pending_connects"],
        serde_json::json!([])
    );
    let connected = state(&mut connection, &mut protocol).await;
    assert_eq!(connected["result"], serde_json::json!([address]));
    assert_eq!(
        connected["document_generation"],
        granted_state["document_generation"]
    );
    let permission = store.list_gateway_permissions(&view).unwrap().remove(0);
    handle
        .revoke_permission(permission.permission_id)
        .await
        .unwrap();
    let revoked = state(&mut connection, &mut protocol).await;
    assert_eq!(revoked["accounts"], serde_json::json!([]));
    assert_eq!(revoked["invalidation_code"], 4100);
    assert!(
        revoked["document_generation"].as_u64().unwrap()
            > granted_state["document_generation"].as_u64().unwrap()
    );
    let revoked_ui = state(&mut connection, &mut protocol).await;
    assert_eq!(revoked_ui["type"], "ui_snapshot");
    assert_eq!(revoked_ui["permissions"], serde_json::json!([]));
    application(&mut connection, &mut protocol, serde_json::json!({"type":"register_document","version":1,"document":"page","url":"https://other.invalid/"})).await;
    let retired = tokio::time::timeout(Duration::from_secs(5), connection.next())
        .await
        .unwrap();
    assert!(!matches!(retired, Some(Ok(Message::Binary(_)))));
    drop(view);
    drop(store);
    handle.shutdown().await.unwrap();
    drop(db);
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root.clone(),
        })
        .unwrap(),
    );
    let restored = GatewayHandle::start(db.clone(), true, 4, GatewayInstallBundle::default());
    let mut snapshots = restored.snapshots();
    snapshots
        .wait_for(|snapshot| snapshot.listener_addr.is_some())
        .await
        .unwrap();
    assert_eq!(snapshots.borrow().peers[0].id, peer);
    assert_eq!(snapshots.borrow().config, config);
    let address = snapshots.borrow().listener_addr.unwrap();
    assert_eq!(
        address,
        SocketAddr::new(config.bind_address, config.port.get())
    );
    let mut connection = socket(address).await;
    let (mut protocol, hello) = Connection::client_reconnect(peer, secret);
    connection
        .send(Message::Binary(hello.into()))
        .await
        .unwrap();
    loop {
        let step = protocol
            .receive_handshake(&read(&mut connection).await)
            .unwrap();
        if let Some(output) = step.outbound {
            connection
                .send(Message::Binary(output.into()))
                .await
                .unwrap();
        }
        if step.event == Some(HandshakeEvent::Authenticated) {
            break;
        }
    }
    let restored_state = state(&mut connection, &mut protocol).await;
    assert_eq!(restored_state["generation"], 4);
    assert_eq!(restored_state["locked"], true);
    idle_ui_snapshot(&mut connection, &mut protocol, 4, true).await;
    restored.revoke_peer(peer).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), connection.next())
        .await
        .unwrap();
    assert!(!matches!(closed, Some(Ok(Message::Binary(_)))));
    assert!(Registry::load(&db).unwrap().peers.is_empty());
    let mut revoked = socket(address).await;
    revoked
        .send(Message::Binary(
            ClientHello::reconnect(peer).encode().into(),
        ))
        .await
        .unwrap();
    let rejected = tokio::time::timeout(Duration::from_secs(5), revoked.next())
        .await
        .unwrap();
    assert!(!matches!(rejected, Some(Ok(Message::Binary(_)))));
    restored
        .configure(GatewayConfig {
            bind_address: Ipv4Addr::UNSPECIFIED.into(),
            ..config
        })
        .await
        .unwrap();
    snapshots
        .wait_for(|snapshot| {
            snapshot
                .listener_addr
                .is_some_and(|address| address.ip().is_unspecified())
        })
        .await
        .unwrap();
    restored.shutdown().await.unwrap();
    // Unknown future storage must not be overwritten by an enable operation.
    let future = br#"{"version":3,"config":{"enabled":true,"bind_address":"127.0.0.1","port":43110},"peers":[]}"#;
    db.put_app_settings_record("gateway-state", future).unwrap();
    let blocked = GatewayHandle::start(db.clone(), true, 5, GatewayInstallBundle::default());
    assert_eq!(
        blocked.configure(config).await,
        Err(GatewayError::InvalidStorage)
    );
    assert_eq!(
        db.get_app_settings_record("gateway-state")
            .unwrap()
            .unwrap(),
        future
    );
    assert!(blocked.snapshots().borrow().listener_addr.is_none());
    blocked.shutdown().await.unwrap();
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn encrypted_reads_suppress_retired_payloads_and_shutdown_waits_for_broker_drain() {
    use super::super::provider::tests::{held_rpc, initialize, state as wallet_state};
    use crate::{HttpContext, RpcChainRoute, RpcOrigin};
    use serde_json::json;
    for remote_error in [false, true] {
        for invalidation in ["lock", "revoke", "restart"] {
            let mut random = [0; 16];
            getrandom::fill(&mut random).unwrap();
            let root = std::env::temp_dir()
                .join(format!("gateway-read-ws-{}", alloy::hex::encode(random)));
            let db = Arc::new(
                DbStore::open(DbConfig {
                    root_dir: root.clone(),
                })
                .unwrap(),
            );
            let store = crate::vault::DesktopVaultStore::from_db(db.clone());
            let view = initialize(&store);
            let (endpoint, started, release, server) = held_rpc(remote_error).await;
            let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
            let config = GatewayConfig {
                enabled: true,
                bind_address: Ipv4Addr::LOCALHOST.into(),
                port: available_port().await,
            };
            handle.configure(config).await.unwrap();
            let address = handle.snapshots().borrow().listener_addr.unwrap();
            let offer = handle.issue_pairing_code().await.unwrap();
            let mut connection = socket(address).await;
            let (mut protocol, peer, secret) = pair(&mut connection, offer.code).await;
            state(&mut connection, &mut protocol).await;
            idle_ui_snapshot(&mut connection, &mut protocol, 1, true).await;
            let origin = RpcOrigin::dapp(
                alloy::hex::encode(peer.to_bytes()),
                "https://wire.invalid/path?q=1#doc",
            )
            .unwrap();
            let account = store
                .list_active_public_accounts_for_session(&view)
                .unwrap()
                .remove(0);
            store
                .grant_gateway_permission(&view, &origin, &account.public_account_uuid, 1)
                .unwrap();
            let permission = store.list_gateway_permissions(&view).unwrap().remove(0);
            let mut wallet = wallet_state(&view);
            wallet.http = Some(HttpContext::direct_for_tests());
            wallet
                .routes
                .insert(1, RpcChainRoute::new(1, vec![endpoint]));
            handle.update_wallet_state(wallet, 2).await.unwrap();
            state(&mut connection, &mut protocol).await;
            idle_ui_snapshot(&mut connection, &mut protocol, 2, false).await;
            application(&mut connection, &mut protocol, json!({"type":"register_document","version":1,"document":"doc","url":origin.web_origin().unwrap().as_str()})).await;
            assert_eq!(
                state(&mut connection, &mut protocol).await["accounts"],
                json!([account.address.to_string()])
            );
            application(&mut connection, &mut protocol, json!({"type":"provider_request","version":1,"document":"doc","request_id":"held","method":"eth_blockNumber","params":[]})).await;
            tokio::time::timeout(Duration::from_secs(5), started.notified())
                .await
                .unwrap();
            match invalidation {
                "lock" => handle
                    .update_wallet_state(super::super::GatewayWalletState::default(), 3)
                    .await
                    .unwrap(),
                "revoke" => handle
                    .revoke_permission(permission.permission_id)
                    .await
                    .unwrap(),
                _ => handle.configure(config).await.unwrap(),
            }
            if invalidation == "restart" {
                drop(connection);
                let address = handle.snapshots().borrow().listener_addr.unwrap();
                connection = socket(address).await;
                let (mut fresh, hello) = Connection::client_reconnect(peer, secret);
                connection
                    .send(Message::Binary(hello.into()))
                    .await
                    .unwrap();
                loop {
                    let step = fresh
                        .receive_handshake(&read(&mut connection).await)
                        .unwrap();
                    if let Some(output) = step.outbound {
                        connection
                            .send(Message::Binary(output.into()))
                            .await
                            .unwrap();
                    }
                    if step.event == Some(HandshakeEvent::Authenticated) {
                        break;
                    }
                }
                protocol = fresh;
                state(&mut connection, &mut protocol).await;
                idle_ui_snapshot(&mut connection, &mut protocol, 2, false).await;
                application(&mut connection, &mut protocol, json!({"type":"register_document","version":1,"document":"doc","url":origin.web_origin().unwrap().as_str()})).await;
                state(&mut connection, &mut protocol).await;
                application(&mut connection, &mut protocol, json!({"type":"provider_request","version":1,"document":"doc","request_id":"fresh","method":"eth_accounts","params":[]})).await;
                let response = state(&mut connection, &mut protocol).await;
                assert_eq!(response["request_id"], "fresh");
                assert_eq!(response["result"], json!([account.address.to_string()]));
            } else {
                loop {
                    let message = state(&mut connection, &mut protocol).await;
                    assert!(!message.to_string().contains("synthetic-owner-payload"));
                    assert_ne!(message["result"], "0xfeed");
                    if message["request_id"] == "held" {
                        assert_eq!(message["error"]["code"], 4100);
                        break;
                    }
                }
            }
            // Session retirement cannot release the accepted broker future. Destructive
            // shutdown acknowledgement remains pending until the held HTTP response settles.
            let mut shutdown = tokio::spawn({
                let handle = handle.clone();
                async move { handle.shutdown().await }
            });
            assert!(
                tokio::time::timeout(Duration::from_millis(30), &mut shutdown)
                    .await
                    .is_err()
            );
            release.notify_one();
            shutdown.await.unwrap().unwrap();
            server.await.unwrap();
            drop(connection);
            drop(handle);
            drop(store);
            drop(view);
            drop(db);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
