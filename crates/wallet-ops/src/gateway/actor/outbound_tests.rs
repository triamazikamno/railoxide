use super::*;
use crate::gateway::{
    GatewayProviderOutcome, GatewayWalletState,
    provider::tests::{authorize, fixture, invalidate_authority},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::DuplexStream;
use tokio_tungstenite::tungstenite::protocol::Role;

fn protocols() -> (Connection, Connection) {
    let secret = [42; 32];
    let (mut client, hello) = Connection::client_reconnect(
        PeerId::from_bytes([8; 16]),
        SessionSecret::from_storage(secret),
    );
    let (mut server, mut flight) = Connection::server(
        ClientHello::decode(&hello).unwrap(),
        ServerAuth::Reconnect(SessionSecret::from_storage(secret)),
    )
    .unwrap();
    loop {
        let step = client.receive_handshake(&flight).unwrap();
        if step.event == Some(HandshakeEvent::Authenticated) {
            break;
        }
        let step = server.receive_handshake(&step.outbound.unwrap()).unwrap();
        flight = step.outbound.unwrap();
    }
    (client, server)
}

async fn session() -> (
    Session<DuplexStream>,
    WebSocketStream<DuplexStream>,
    Connection,
) {
    // Fits one ciphertext frame. The next frame cannot flush until the test reads.
    let (server_io, client_io) = tokio::io::duplex(MAX_RECORD_LEN + 16);
    let (client_protocol, server_protocol) = protocols();
    let socket = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
    let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
    let session = Session {
        socket,
        outbound: VecDeque::new(),
        outbound_bytes: 0,
        protocol: Some(server_protocol),
        challenge: None,
        deadline: Instant::now() + HANDSHAKE_TIMEOUT,
        heartbeat: Instant::now() + HEARTBEAT,
    };
    (session, client, client_protocol)
}

async fn flush_output(session: &mut Session<DuplexStream>, id: u64, provider: &mut DappProvider) {
    while !session.outbound.is_empty() {
        if let Some(ticket) = poll_fn(|cx| session.poll_output(id, provider, cx))
            .await
            .unwrap()
        {
            provider.delivered(ticket);
        }
    }
}

async fn receive(client: &mut WebSocketStream<DuplexStream>, protocol: &mut Connection) -> Value {
    loop {
        let Message::Binary(bytes) = client.next().await.unwrap().unwrap() else {
            panic!("binary record");
        };
        if let Some(bytes) = protocol.receive_frame(&bytes, 0).unwrap() {
            return serde_json::from_slice(&bytes).unwrap();
        }
    }
}

fn enqueue(session: &mut Session<DuplexStream>, provider: &mut DappProvider, id: u64) {
    for (owner, delivery) in provider.drain() {
        assert_eq!(owner, id);
        session.deliver(delivery).unwrap();
    }
}

#[tokio::test]
async fn small_output_finishes_before_the_next_wallet_transition() {
    for snapshot in [false, true] {
        let (path, mut provider, view) = fixture();
        let (mut writer, mut client, mut protocol) = session().await;
        provider.attach_ui_peer(1, PeerId::from_bytes([8; 16]));
        if snapshot {
            let (_, delivery) = provider
                .drain()
                .into_iter()
                .find(|(_, delivery)| {
                    matches!(delivery.message, GatewayServerMessage::UiSnapshot { .. })
                })
                .unwrap();
            writer.deliver(delivery).unwrap();
        } else {
            writer
                .application(GatewayServerMessage::State {
                    version: 1,
                    generation: 1,
                    locked: false,
                    wallet_transition: false,
                })
                .unwrap();
        }
        // A ready socket must commit its small message before another actor turn.
        poll_fn(|cx| writer.poll_output(1, &provider, cx))
            .await
            .unwrap();
        invalidate_authority(&provider, true);
        let heartbeat = protocol
            .seal_message(br#"{"type":"heartbeat","version":1}"#)
            .unwrap();
        for frame in heartbeat {
            client.send(Message::Binary(frame.into())).await.unwrap();
        }
        let mut sessions = HashMap::from([(1, writer)]);
        let mut cursor = ProgressCursor::default();
        let Progress::Incoming(1, Some(Ok(Message::Binary(input)))) =
            poll_fn(|cx| cursor.poll(&mut sessions, &mut provider, cx)).await
        else {
            panic!("live input must survive the wallet transition");
        };
        let mut writer = sessions.remove(&1).unwrap();
        let input = writer
            .protocol
            .as_mut()
            .unwrap()
            .receive_frame(&input, 0)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&input).unwrap()["type"],
            "heartbeat"
        );
        provider.update_wallet(GatewayWalletState::default(), 2);
        writer
            .application(GatewayServerMessage::State {
                version: 1,
                generation: 2,
                locked: true,
                wallet_transition: true,
            })
            .unwrap();
        writer
            .application(GatewayServerMessage::Heartbeat { version: 1 })
            .unwrap();
        let ((), messages) = tokio::join!(flush_output(&mut writer, 1, &mut provider), async {
            let mut messages = Vec::new();
            for _ in 0..3 {
                messages.push(receive(&mut client, &mut protocol).await);
            }
            messages
        });
        assert_eq!(messages[0]["generation"], 1);
        assert_eq!(messages[1]["generation"], 2);
        assert_eq!(messages[1]["locked"], true);
        assert_eq!(messages[2]["type"], "heartbeat");
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn fragmented_output_yields_to_invalidation_and_another_session() {
    for invalidation in [
        "none",
        "lock",
        "authority",
        "input_authority",
        "revoke",
        "unregister",
        "deadline",
    ] {
        let (path, mut provider, view) = fixture();
        let (_, permission) =
            authorize(&mut provider, &view, "http://127.0.0.1:1".parse().unwrap());
        let (mut writer, mut client, mut protocol) = session().await;
        let (mut other, mut other_client, mut other_protocol) = session().await;
        let now = Instant::now()
            - if invalidation == "deadline" {
                Duration::from_secs(29)
            } else {
                Duration::ZERO
            };
        provider
            .request(
                1,
                "doc".to_owned(),
                "fragmented".to_owned(),
                "eth_accounts",
                json!([]),
                now,
            )
            .unwrap();
        let (_, mut delivery) = provider.drain().pop().unwrap();
        let payload = "synthetic-sensitive-payload".repeat(12_000);
        let GatewayServerMessage::ProviderResponse { outcome, .. } = &mut delivery.message else {
            panic!("response");
        };
        *outcome = GatewayProviderOutcome::Success {
            result: json!(payload),
        };
        writer.deliver(delivery).unwrap();
        // Flush one complete fragment, then stop consuming the socket.
        poll_fn(|cx| writer.poll_output(1, &provider, cx))
            .await
            .unwrap();
        let Message::Binary(first) = client.next().await.unwrap().unwrap() else {
            panic!("first fragment");
        };
        assert!(protocol.receive_frame(&first, 0).unwrap().is_none());
        // Drive until a flush is pending on the bounded duplex buffer.
        poll_fn(|cx| {
            for _ in 0..8 {
                match writer.poll_output(1, &provider, cx) {
                    Poll::Pending => return Poll::Ready(()),
                    Poll::Ready(result) => {
                        result.unwrap();
                    }
                }
            }
            panic!("large response must reach backpressure");
        })
        .await;
        if invalidation == "input_authority" {
            client.send(Message::Ping(vec![1].into())).await.unwrap();
            // Reading the ping queues an automatic pong beside the blocked ciphertext.
            assert!(matches!(
                writer.socket.next().await.unwrap().unwrap(),
                Message::Ping(_)
            ));
            // Keep input ready: without the input guard, read attempts the pending
            // flush, then returns this ping instead of falling through to output.
            client.send(Message::Ping(vec![2].into())).await.unwrap();
        }
        match invalidation {
            "authority" | "input_authority" => invalidate_authority(&provider, true),
            "lock" => provider.update_wallet(GatewayWalletState::default(), 3),
            "revoke" => provider.revoke(&permission.permission_id).unwrap(),
            "unregister" => provider.unregister(1, "doc"),
            "deadline" => {
                // Keep the actual ticket deadline. Move time after transmission began.
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(2)).await;
            }
            _ => {}
        }
        // This peer is serviceable while the first transport is still backpressured.
        other
            .application(GatewayServerMessage::Heartbeat { version: 1 })
            .unwrap();
        let ((), message) = tokio::join!(
            flush_output(&mut other, 2, &mut provider),
            receive(&mut other_client, &mut other_protocol)
        );
        assert_eq!(message["type"], "heartbeat");
        if invalidation == "none" {
            let ((), response) = tokio::join!(
                flush_output(&mut writer, 1, &mut provider),
                receive(&mut client, &mut protocol)
            );
            assert_eq!(response["result"], payload);
        } else {
            if invalidation == "input_authority" {
                let mut sessions = HashMap::from([(1, writer)]);
                let mut cursor = ProgressCursor::default();
                assert!(matches!(
                    poll_fn(|cx| cursor.poll(&mut sessions, &mut provider, cx)).await,
                    Progress::Write(1, Err(GatewayError::Unavailable))
                ));
                writer = sessions.remove(&1).unwrap();
            } else {
                assert!(matches!(
                    poll_fn(|cx| writer.poll_output(1, &provider, cx)).await,
                    Err(GatewayError::Unavailable)
                ));
            }
            drop(writer);
            // Already-written fragments may arrive, but cannot form the retired message.
            while let Some(Ok(Message::Binary(bytes))) = client.next().await {
                assert!(protocol.receive_frame(&bytes, 0).unwrap().is_none());
            }
        }
        if invalidation == "deadline" {
            tokio::time::resume();
        }
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn expired_queued_read_uses_control_delivery_without_retiring_other_documents() {
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, "http://127.0.0.1:1".parse().unwrap());
    provider
        .register(
            1,
            PeerId::from_bytes([8; 16]),
            "other".to_owned(),
            "https://other.invalid/",
        )
        .unwrap();
    provider.drain();
    let now = Instant::now();
    for index in 0..32 {
        provider
            .request(
                1,
                "doc".to_owned(),
                format!("local-{index}"),
                "eth_accounts",
                json!([]),
                now,
            )
            .unwrap();
    }
    provider
        .request(
            1,
            "doc".to_owned(),
            "queued".to_owned(),
            "eth_blockNumber",
            json!([]),
            now,
        )
        .unwrap();
    assert!(provider.jobs.is_empty());
    tokio::time::advance(Duration::from_secs(31)).await;
    provider.tick(Instant::now());
    let (mut writer, mut client, mut protocol) = session().await;
    enqueue(&mut writer, &mut provider, 1);
    let ((), responses) = tokio::join!(flush_output(&mut writer, 1, &mut provider), async {
        let mut responses = Vec::new();
        for _ in 0..33 {
            responses.push(receive(&mut client, &mut protocol).await);
        }
        responses
    });
    assert!(
        responses
            .iter()
            .any(|message| message["request_id"] == "queued" && message["error"]["code"] == -32002)
    );
    assert!(provider.jobs.is_empty());
    provider
        .request(
            1,
            "other".to_owned(),
            "still-live".to_owned(),
            "eth_accounts",
            json!([]),
            Instant::now(),
        )
        .unwrap();
    enqueue(&mut writer, &mut provider, 1);
    let ((), response) = tokio::join!(
        flush_output(&mut writer, 1, &mut provider),
        receive(&mut client, &mut protocol)
    );
    assert_eq!(response["request_id"], "still-live");
    assert_eq!(response["result"], json!([]));
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn unsealed_state_accounts_and_approval_labels_are_discarded_on_lock() {
    for authority_only in [false, true] {
        let (path, mut provider, view) = fixture();
        let (peer, _) = authorize(&mut provider, &view, "http://127.0.0.1:1".parse().unwrap());
        provider.attach_ui_peer(1, peer);
        let mut wallet = provider.authority().borrow().clone();
        wallet.private_view_supported = true;
        wallet.private_view = Some(crate::gateway::GatewayPrivateView {
            total: Some("synthetic-private-total".into()),
            ..Default::default()
        });
        provider.update_wallet(wallet, 2);
        provider
            .register(
                1,
                peer,
                "doc".to_owned(),
                "https://reads.invalid/path?q=1#doc",
            )
            .unwrap();
        provider
            .register(1, peer, "approval".to_owned(), "https://approval.invalid/")
            .unwrap();
        provider
            .request(
                1,
                "approval".to_owned(),
                "connect".to_owned(),
                "eth_requestAccounts",
                json!([]),
                Instant::now(),
            )
            .unwrap();
        let (mut writer, mut client, mut protocol) = session().await;
        enqueue(&mut writer, &mut provider, 1);
        writer
            .application(GatewayServerMessage::State {
                version: 1,
                locked: false,
                wallet_transition: false,
                generation: 2,
            })
            .unwrap();
        provider
            .request(
                1,
                "doc".to_owned(),
                "accounts".to_owned(),
                "eth_accounts",
                json!([]),
                Instant::now(),
            )
            .unwrap();
        enqueue(&mut writer, &mut provider, 1);
        if authority_only {
            invalidate_authority(&provider, true);
        } else {
            provider.update_wallet(GatewayWalletState::default(), 3);
            enqueue(&mut writer, &mut provider, 1);
        }
        let reader = async {
            let mut messages = Vec::new();
            while let Some(Ok(Message::Binary(bytes))) = client.next().await {
                if let Some(bytes) = protocol.receive_frame(&bytes, 0).unwrap() {
                    messages.push(serde_json::from_slice::<Value>(&bytes).unwrap());
                }
            }
            messages
        };
        let ((), messages) = tokio::join!(
            async {
                flush_output(&mut writer, 1, &mut provider).await;
                drop(writer);
            },
            reader
        );
        assert!(!messages.is_empty());
        for message in messages {
            if authority_only {
                assert_eq!(message["type"], "provider_response");
                assert_eq!(message["request_id"], "accounts");
                assert_eq!(message["error"]["code"], 4100);
            } else {
                assert_eq!(message["generation"], 3);
                if message["type"] == "provider_state" {
                    assert_eq!(message["accounts"], json!([]));
                }
                if message["type"] == "ui_snapshot" {
                    assert_eq!(message["locked"], true);
                    assert!(message["private_view"].is_null());
                    assert_eq!(message["accounts"], json!([]));
                    for prompt in message["pending_connects"].as_array().unwrap() {
                        assert_eq!(prompt["accounts"], json!([]));
                    }
                }
            }
        }
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn ready_noop_input_cannot_exclude_other_progress_sources_or_sessions() {
    let (path, mut provider, view) = fixture();
    let (first, mut first_client, mut first_protocol) = session().await;
    let (mut second, mut second_client, mut second_protocol) = session().await;
    second
        .application(GatewayServerMessage::Heartbeat { version: 1 })
        .unwrap();
    let noop = serde_json::to_vec(&json!({"type":"resolve_connect", "version":1,
        "request_id":"unknown-approval", "public_account_uuid":null, "chain_id":1}))
    .unwrap();
    // More input is buffered than the bounded polling window below can consume.
    for _ in 0..16 {
        for frame in first_protocol.seal_message(&noop).unwrap() {
            first_client
                .send(Message::Binary(frame.into()))
                .await
                .unwrap();
        }
    }
    for frame in second_protocol.seal_message(&noop).unwrap() {
        second_client
            .send(Message::Binary(frame.into()))
            .await
            .unwrap();
    }
    let (ready, completed) = tokio::sync::oneshot::channel();
    provider.jobs.spawn(async move {
        let _ = ready.send(());
        (
            u64::MAX,
            Err(crate::gateway::reads::ReadError::Broker(
                crate::RpcBrokerError::InvalidResponse,
            )),
        )
    });
    completed.await.unwrap();
    let mut sessions = HashMap::from([(1, first), (2, second)]);
    let mut cursor = ProgressCursor::default();
    let mut completed = false;
    let mut second_input = false;
    let mut first_inputs = 0;
    let mut output_finished = false;
    for _ in 0..12 {
        match poll_fn(|cx| cursor.poll(&mut sessions, &mut provider, cx)).await {
            Progress::Incoming(id, Some(Ok(Message::Binary(bytes)))) => {
                let session = sessions.get_mut(&id).unwrap();
                let message = session
                    .protocol
                    .as_mut()
                    .unwrap()
                    .receive_frame(&bytes, 0)
                    .unwrap()
                    .unwrap();
                let GatewayClientMessage::ResolveConnect {
                    request_id,
                    public_account_uuid,
                    chain_id,
                    ..
                } = serde_json::from_slice(&message).unwrap()
                else {
                    panic!("no-op approval response");
                };
                provider.resolve_connect(
                    id,
                    session.peer().unwrap(),
                    &request_id,
                    public_account_uuid.as_deref(),
                    chain_id,
                    Instant::now(),
                );
                if id == 1 {
                    first_inputs += 1;
                } else {
                    second_input = true;
                }
            }
            Progress::Read(Ok((id, _))) => {
                assert_eq!(id, u64::MAX);
                completed = true;
            }
            Progress::Write(2, Ok(_)) => {
                output_finished = sessions[&2].outbound.is_empty();
            }
            _ => panic!("unexpected transport progress"),
        }
        if completed && second_input && output_finished {
            break;
        }
    }
    assert!(completed, "ready broker completion was starved by input");
    assert!(second_input, "another session's input was starved");
    assert!(output_finished, "queued output was starved by input");
    assert!(
        first_inputs > 0 && first_inputs < 16,
        "the input flood must still be ready"
    );
    assert_eq!(
        receive(&mut second_client, &mut second_protocol).await["type"],
        "heartbeat"
    );
    assert!(provider.drain().is_empty());
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}
