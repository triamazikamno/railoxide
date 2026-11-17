use super::super::{DeliveryStatus, tests::fixture};
use super::*;
use crate::settings::{WalletSettings, chain_editor_snapshot};

#[test]
fn editor_reopens_beyond_old_session_limit_without_allowing_replay() {
    let (path, mut provider, _) = fixture();
    let peer = PeerId::from_bytes([7; 16]);
    provider.attach_ui_peer(1, peer);
    for sequence in 1..=256 {
        let view_id = sequence.to_string();
        let request = provider
            .chain_editor_command(
                1,
                peer,
                1,
                GatewayChainEditorCommand::Open {
                    view_id: view_id.clone(),
                    request_id: "open".into(),
                },
            )
            .expect("closing an editor must leave room for the next open");
        provider.chain_editor_command(1, peer, 1, GatewayChainEditorCommand::Close { view_id });
        assert!(!request.is_current());
    }
    provider.chain_editor_views.retire_context();
    for replay in ["1", "128", "256", "0257"] {
        assert!(
            provider
                .chain_editor_command(
                    1,
                    peer,
                    1,
                    GatewayChainEditorCommand::Open {
                        view_id: replay.into(),
                        request_id: "replay".into(),
                    },
                )
                .is_none()
        );
    }
    drop(provider);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn editor_authority_bounds_commands_and_retires_credential_deliveries() {
    let (path, mut provider, _) = fixture();
    let peer = PeerId::from_bytes([7; 16]);
    provider.attach_ui_peer(1, peer);
    provider.attach_ui_peer(2, peer);
    let open = |view: &str| GatewayChainEditorCommand::Open {
        view_id: view.into(),
        request_id: "open".into(),
    };
    assert!(
        provider
            .chain_editor_command(3, peer, 1, open("1"))
            .is_none()
    );
    assert!(
        provider
            .chain_editor_command(1, peer, 0, open("1"))
            .is_none()
    );
    let request = provider
        .chain_editor_command(1, peer, 1, open("1"))
        .unwrap();
    let mut settings = WalletSettings::default();
    settings.chains.per_chain.get_mut(&1).unwrap().rpc_endpoints =
        vec!["https://synthetic:credential@rpc.example".to_owned().into()];
    let snapshot = chain_editor_snapshot(&settings, Some(1)).unwrap();
    let revision = snapshot.revision.clone();
    provider.complete_chain_editor_request(request, GatewayChainEditorOutcome::Ready { snapshot });
    let mut deliveries = provider.drain();
    let (_, mut reply) = deliveries.pop().unwrap();
    assert!(matches!(
        reply.message,
        GatewayServerMessage::ChainEditor { .. }
    ));
    assert!(
        !serde_json::to_string(&provider.ui(1))
            .unwrap()
            .contains("credential")
    );
    assert!(
        !serde_json::to_string(&provider.ui(2))
            .unwrap()
            .contains("credential")
    );
    assert!(provider.delivery(1, &mut reply) == DeliveryStatus::Current);
    let command = |request_id: &str, chain: &str| GatewayChainEditorCommand::Run {
        view_id: "1".into(),
        request_id: request_id.into(),
        revision: revision.clone(),
        command: ChainEditorCommand::Reset {
            chain_id: chain.into(),
        },
    };
    assert!(
        provider
            .chain_editor_command(2, peer, 1, command("foreign", "1"))
            .is_none()
    );
    assert!(
        provider
            .chain_editor_command(1, peer, 1, command("rebound", "56"))
            .is_none()
    );
    let reset = provider
        .chain_editor_command(1, peer, 1, command("reset", "1"))
        .unwrap();
    assert!(
        provider
            .chain_editor_command(1, peer, 1, command("reset", "1"))
            .is_none()
    );
    assert!(
        provider.delivery(1, &mut reply) == DeliveryStatus::Discard,
        "a superseded queued reply cannot restore an older draft"
    );
    provider.chain_editor_command(
        1,
        peer,
        1,
        GatewayChainEditorCommand::Close {
            view_id: "1".into(),
        },
    );
    assert!(!reset.is_current());
    provider.complete_chain_editor_request(
        reset,
        GatewayChainEditorOutcome::Failed {
            message: "late".into(),
        },
    );
    assert!(provider.drain().is_empty());
    assert!(
        provider
            .chain_editor_command(1, peer, 1, open("1"))
            .is_none(),
        "closed view IDs cannot be replayed"
    );
    let request = provider
        .chain_editor_command(1, peer, 1, open("2"))
        .unwrap();
    provider.complete_chain_editor_request(
        request,
        GatewayChainEditorOutcome::Ready {
            snapshot: chain_editor_snapshot(&settings, Some(1)).unwrap(),
        },
    );
    let (_, mut queued) = provider.drain().pop().unwrap();
    // A Test is bound to the view's chain and cannot outlive the view.
    let probe = |request_id: &str, chain: &str| {
        let mut draft = railgun_ui::chain_editor::ChainDraft::new();
        draft.chain_id = chain.into();
        GatewayChainEditorCommand::Run {
            view_id: "2".into(),
            request_id: request_id.into(),
            revision: revision.clone(),
            command: ChainEditorCommand::Probe { draft },
        }
    };
    assert!(
        provider
            .chain_editor_command(1, peer, 1, probe("probe-rebound", "56"))
            .is_none()
    );
    let probing = provider
        .chain_editor_command(1, peer, 1, probe("probe", "1"))
        .unwrap();
    provider.chain_editor_command(
        1,
        peer,
        1,
        GatewayChainEditorCommand::Close {
            view_id: "2".into(),
        },
    );
    provider.complete_chain_editor_request(
        probing,
        GatewayChainEditorOutcome::Probed {
            probe: NativeUsdProbe::new(Err("Could not read the oracle".to_owned())),
        },
    );
    assert!(provider.drain().is_empty());
    provider.update_wallet(GatewayWalletState::default(), 2);
    assert!(provider.delivery(1, &mut queued) == DeliveryStatus::Discard);
    drop(provider);
    std::fs::remove_dir_all(path).unwrap();
}
