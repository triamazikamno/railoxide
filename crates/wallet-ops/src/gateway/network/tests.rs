use super::super::tests::{fixture, messages, snapshot};
use super::*;
use crate::gateway::{GatewayClientMessage, GatewayUiEventKind};

fn run(view: &str, request: &str) -> GatewayNetworkCommand {
    GatewayNetworkCommand::Run {
        view_id: view.into(),
        request_id: request.into(),
        operation: GatewayNetworkOperation::QueryExitIp,
    }
}

#[test]
fn commands_reject_overrides_and_require_a_live_current_view() {
    let wire = serde_json::json!({"type":"network", "version":1, "generation":1, "context_revision":"context-a", "command":{"action":"run", "view_id":"popup", "request_id":"query", "operation":"query_exit_ip"}});
    assert!(serde_json::from_value::<GatewayClientMessage>(wire.clone()).is_ok());
    for name in ["endpoint", "proxy_url", "path"] {
        let mut invalid = wire.clone();
        invalid["command"][name] = "caller-override".into();
        assert!(serde_json::from_value::<GatewayClientMessage>(invalid).is_err());
    }
    let (path, mut provider, _) = fixture();
    let peer = PeerId::from_bytes([7; 16]);
    provider.attach_ui_peer(1, peer);
    provider.attach_ui_peer(2, peer);
    assert!(
        provider
            .network_command(1, peer, 1, "context-a", run("popup", "query"))
            .is_none()
    );
    let old = provider.wallet.clone();
    let mut wallet = old.clone();
    wallet.network_view = Some(GatewayNetworkView::new(
        "context-a".into(),
        &WalletNetworkHealth::new(
            WalletNetworkMode::Tor,
            WalletNetworkHealthState::Ready,
            "internal SOCKS URL must not be exported",
        ),
        None,
        None,
    ));
    assert!(old.same_authority(&wallet));
    assert!(!old.same_state(&wallet));
    assert_eq!(wallet.network_view.as_ref().unwrap().detail, "");
    provider.update_wallet(wallet.clone(), 1);
    for (session, view) in [(1, "popup"), (2, "panel")] {
        let _ = provider.network_command(
            session,
            peer,
            1,
            "context-a",
            GatewayNetworkCommand::Open {
                view_id: view.into(),
            },
        );
    }
    for (session, sender, generation, revision, view) in [
        (1, PeerId::from_bytes([8; 16]), 1, "context-a", "popup"),
        (2, peer, 1, "context-a", "popup"),
        (1, peer, 0, "context-a", "popup"),
        (1, peer, 1, "context-b", "popup"),
    ] {
        assert!(
            provider
                .network_command(session, sender, generation, revision, run(view, "query"))
                .is_none()
        );
    }
    let first = provider
        .network_command(1, peer, 1, "context-a", run("popup", "query"))
        .unwrap();
    assert!(
        provider
            .network_command(1, peer, 1, "context-a", run("popup", "query"))
            .is_none()
    );
    let second = provider
        .network_command(2, peer, 1, "context-a", run("panel", "query"))
        .unwrap();
    let live = Arc::new(AtomicBool::new(true));
    let event = provider
        .ui_event(
            1,
            GatewayUiEventKind::Network {
                request: first.clone(),
            },
            &live,
        )
        .unwrap();
    wallet.network_view.as_mut().unwrap().download_rate = Some(0);
    assert!(provider.wallet.same_authority(&wallet));
    assert!(event.is_current(&wallet, 1));
    provider.update_wallet(wallet.clone(), 1);
    provider.complete_network_request(
        first.clone(),
        GatewayNetworkOutcome::ExitIp {
            ip: "203.0.113.7".parse().unwrap(),
        },
    );
    assert_eq!(provider.network_results(1).len(), 1);
    assert!(provider.network_results(2).is_empty());
    assert!(
        provider
            .network_command(1, peer, 1, "context-a", run("popup", "query"))
            .is_none()
    );
    let cancelled = provider
        .network_command(1, peer, 1, "context-a", run("popup", "cancelled"))
        .unwrap();
    let _ = provider.network_command(
        1,
        peer,
        1,
        "context-a",
        GatewayNetworkCommand::CancelQuery {
            view_id: "popup".into(),
        },
    );
    assert!(!cancelled.is_current("context-a"));
    provider.complete_network_request(cancelled, GatewayNetworkOutcome::Done);
    assert!(provider.network_results(1).is_empty());
    let closing = provider
        .network_command(1, peer, 1, "context-a", run("popup", "closing"))
        .unwrap();
    assert!(closing.is_current("context-a"));
    let _ = provider.network_command(
        1,
        peer,
        1,
        "context-a",
        GatewayNetworkCommand::Close {
            view_id: "popup".into(),
        },
    );
    assert!(!closing.is_current("context-a"));
    assert!(!event.is_current(&wallet, 1));
    assert!(second.is_current("context-a"));
    let _ = provider.network_command(
        1,
        peer,
        1,
        "context-a",
        GatewayNetworkCommand::Open {
            view_id: "popup".into(),
        },
    );
    assert!(
        provider
            .network_command(1, peer, 1, "context-a", run("popup", "late"))
            .is_none()
    );
    provider.complete_network_request(first, GatewayNetworkOutcome::Done);
    assert!(provider.network_results(1).is_empty());
    wallet.network_view.as_mut().unwrap().context_revision = "context-b".into();
    assert!(!second.is_current("context-b"));
    provider.update_wallet(wallet, 1);
    provider.complete_network_request(second, GatewayNetworkOutcome::Done);
    assert!(provider.network_results(2).is_empty());
    provider.update_wallet(GatewayWalletState::default(), 2);
    provider.push_ui(1);
    let output = messages(&mut provider);
    let locked = snapshot(&output);
    assert!(locked.get("network_view").is_none());
    assert!(locked.get("network_results").is_none());
    drop(provider);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn admitted_tor_rotation_is_shared_and_retired_views_get_no_late_result() {
    let (path, mut provider, _) = fixture();
    let peer = PeerId::from_bytes([7; 16]);
    provider.attach_ui_peer(1, peer);
    provider.attach_ui_peer(2, peer);
    let tor = |revision: &str, rotation| {
        let mut view = GatewayNetworkView::new(
            revision.into(),
            &WalletNetworkHealth::new(WalletNetworkMode::Tor, WalletNetworkHealthState::Ready, ""),
            None,
            None,
        );
        view.rotation = rotation;
        view
    };
    let new_session = |view: &str, request: &str| GatewayNetworkCommand::Run {
        view_id: view.into(),
        request_id: request.into(),
        operation: GatewayNetworkOperation::NewTorSession,
    };
    let mut wallet = provider.wallet.clone();
    wallet.network_view = Some(tor("context-a", None));
    provider.update_wallet(wallet.clone(), 1);
    for (session, view) in [(1, "popup"), (2, "panel")] {
        let _ = provider.network_command(
            session,
            peer,
            1,
            "context-a",
            GatewayNetworkCommand::Open {
                view_id: view.into(),
            },
        );
    }

    // Each view admits one pending request; the desktop joins both to its single rotation.
    let first = provider
        .network_command(1, peer, 1, "context-a", new_session("popup", "first"))
        .unwrap();
    assert!(
        provider
            .network_command(1, peer, 1, "context-a", new_session("popup", "again"))
            .is_none()
    );
    let second = provider
        .network_command(2, peer, 1, "context-a", new_session("panel", "second"))
        .unwrap();
    let _ = messages(&mut provider);
    wallet.network_view = Some(tor("context-a", Some(GatewayTorRotation::InProgress)));
    provider.update_wallet(wallet.clone(), 1);
    assert!(first.is_current("context-a"));
    provider.push_ui(2);
    let output = messages(&mut provider);
    assert_eq!(
        snapshot(&output)["network_view"]["rotation"],
        serde_json::json!({"state": "in_progress"})
    );

    // Closing the initiating view retires only its presentation.
    let _ = provider.network_command(
        1,
        peer,
        1,
        "context-a",
        GatewayNetworkCommand::Close {
            view_id: "popup".into(),
        },
    );
    assert!(!first.is_current("context-a"));
    assert!(second.is_current("context-a"));

    // Waku fails after the session advanced: no request result survives the new revision,
    // and the snapshot carries the failure to current views.
    let _ = messages(&mut provider);
    wallet.network_view = Some(tor(
        "context-b",
        Some(GatewayTorRotation::Failed {
            error: GatewayNetworkError::WakuUnavailable,
        }),
    ));
    provider.update_wallet(wallet, 1);
    for request in [first, second] {
        provider.complete_network_request(
            request,
            GatewayNetworkOutcome::Failed {
                error: GatewayNetworkError::WakuUnavailable,
            },
        );
    }
    assert!(provider.network_results(1).is_empty());
    assert!(provider.network_results(2).is_empty());
    provider.push_ui(2);
    let output = messages(&mut provider);
    assert_eq!(
        snapshot(&output)["network_view"]["rotation"],
        serde_json::json!({
            "state": "failed",
            "error": GatewayNetworkError::WakuUnavailable.message(),
        })
    );
    drop(provider);
    std::fs::remove_dir_all(path).unwrap();
}
