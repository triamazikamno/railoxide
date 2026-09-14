use super::*;
use crate::gateway::{
    GatewayClientMessage, GatewayPrivateCommand, GatewayPrivateView, GatewayPrivateWallet,
    GatewayUiEventKind,
};

#[test]
fn private_selection_requires_capability_visible_target_live_peer_and_current_transition() {
    let (path, mut provider, view) = fixture();
    let peer = PeerId::from_bytes([7; 16]);
    provider.attach_ui_peer(1, peer);
    let command = || GatewayPrivateCommand::SelectWallet {
        wallet_id: "visible".into(),
    };
    assert!(provider.private_command(1, peer, 1, command()).is_none());
    let mut wallet = provider.wallet.clone();
    wallet.private_view_supported = true;
    wallet.private_view = Some(GatewayPrivateView {
        wallets: vec![
            GatewayPrivateWallet {
                wallet_id: "visible".into(),
                ..Default::default()
            },
            GatewayPrivateWallet {
                wallet_id: "hardware-device:ledger".into(),
                hardware: Some("ledger".into()),
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    assert!(provider.wallet.same_authority(&wallet));
    assert!(!provider.wallet.same_state(&wallet));
    provider.update_wallet(wallet.clone(), 1);
    assert!(provider.private_command(1, peer, 1, command()).is_some());
    let hardware_command = || GatewayPrivateCommand::SelectWallet {
        wallet_id: "hardware-device:ledger".into(),
    };
    assert!(
        provider
            .private_command(1, peer, 1, hardware_command())
            .is_some()
    );
    for wallet_id in ["ledger-profile", "hardware-device:trezor"] {
        assert!(
            provider
                .private_command(
                    1,
                    peer,
                    1,
                    GatewayPrivateCommand::SelectWallet {
                        wallet_id: wallet_id.into(),
                    }
                )
                .is_none()
        );
    }
    assert!(
        provider
            .private_command(1, PeerId::from_bytes([8; 16]), 1, command())
            .is_none()
    );
    assert!(provider.private_command(2, peer, 1, command()).is_none());
    assert!(provider.private_command(1, peer, 0, command()).is_none());
    assert!(
        provider
            .private_command(
                1,
                peer,
                1,
                GatewayPrivateCommand::SelectWallet {
                    wallet_id: "concealed".into()
                }
            )
            .is_none()
    );

    let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let event = provider
        .ui_event(
            1,
            GatewayUiEventKind::PrivateView { command: command() },
            &live,
        )
        .unwrap();
    assert!(event.is_current(&wallet, 1));
    wallet.wallet_selection_generation += 1;
    provider
        .authority_fallback
        .as_ref()
        .unwrap()
        .send_replace(wallet.clone());
    assert!(!event.is_current(&wallet, 1));
    assert!(provider.private_command(1, peer, 1, command()).is_none());
    // Metadata changes can revoke visibility without replacing the wallet capability.
    wallet.wallet_selection_generation = provider.wallet.wallet_selection_generation;
    wallet
        .private_view
        .as_mut()
        .unwrap()
        .wallets
        .retain(|choice| choice.wallet_id == "visible");
    provider
        .authority_fallback
        .as_ref()
        .unwrap()
        .send_replace(wallet.clone());
    assert!(provider.private_command(1, peer, 1, command()).is_some());
    assert!(
        provider
            .private_command(1, peer, 1, hardware_command())
            .is_none()
    );
    wallet.private_view.as_mut().unwrap().wallets.clear();
    provider
        .authority_fallback
        .as_ref()
        .unwrap()
        .send_replace(wallet);
    assert!(provider.private_command(1, peer, 1, command()).is_none());
    provider.update_wallet(GatewayWalletState::default(), 2);
    assert!(!event.is_current(&provider.wallet, 2));
    assert!(provider.private_command(1, peer, 2, command()).is_none());
    drop(event);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn private_command_keeps_the_existing_public_wire_input_compatible() {
    assert!(matches!(
        serde_json::from_value::<GatewayClientMessage>(json!({
            "type": "public_view", "version": 1, "generation": 3,
            "command": { "type": "select_chain", "chain_id": 1 }
        }))
        .unwrap(),
        GatewayClientMessage::PublicView { .. }
    ));
    assert!(matches!(
        serde_json::from_value::<GatewayClientMessage>(json!({
            "type": "private_view", "version": 1, "generation": 3,
            "command": { "type": "select_wallet", "wallet_id": "visible" }
        }))
        .unwrap(),
        GatewayClientMessage::PrivateView { .. }
    ));
    assert!(
        serde_json::from_value::<GatewayClientMessage>(json!({
            "type": "public_view", "version": 1, "generation": 3,
            "command": { "type": "select_wallet", "wallet_id": "visible" }
        }))
        .is_err()
    );
}

#[test]
fn oversized_private_presentation_is_unavailable_without_truncating_totals() {
    let (path, mut provider, view) = fixture();
    provider.attach_ui_peer(1, PeerId::from_bytes([7; 16]));
    let mut wallet = provider.wallet.clone();
    wallet.private_view_supported = true;
    wallet.private_view = Some(GatewayPrivateView {
        total: Some("$42.00".into()),
        assets: vec![crate::gateway::GatewayPrivateAsset {
            symbol: "x".repeat(dapp_gateway_protocol::MAX_MESSAGE_LEN),
            ..Default::default()
        }],
        ..Default::default()
    });
    provider.update_wallet(wallet, 1);
    let value = serde_json::to_value(provider.ui(1)).unwrap();
    assert!(value["private_view"].is_null());
    assert!(value["ui_error"].is_string());
    assert!(serde_json::to_vec(&value).unwrap().len() < dapp_gateway_protocol::MAX_MESSAGE_LEN);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}
