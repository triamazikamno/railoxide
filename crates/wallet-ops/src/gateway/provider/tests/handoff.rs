use super::*;
use crate::gateway::{GatewayUiEventKind, GatewayWalletSwitchTransition};
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
async fn native_summaries_are_peer_scoped_and_lock_purges_stale_work_and_terminal_ui() {
    let (path, mut provider, view) = fixture();
    authorize(
        &mut provider,
        &view,
        url::Url::parse("http://127.0.0.1:1").unwrap(),
    );
    provider
        .register(
            2,
            PeerId::from_bytes([9; 16]),
            "other".into(),
            "https://reads.invalid/path?q=1#doc",
        )
        .unwrap();
    messages(&mut provider);
    personal_approval(&mut provider, "sign", Instant::now());
    let ready = provider.approval_updates.borrow()[0].clone();
    provider.publish_summaries(
        &provider.wallet.clone(),
        provider.generation,
        vec![(ready.id.clone(), "Native intent summary".into())],
    );
    let ui = serde_json::to_value(provider.ui(1)).unwrap();
    assert_eq!(
        ui["pending_requests"][0]["summary"],
        "Native intent summary"
    );
    assert_eq!(
        serde_json::to_value(provider.ui(2)).unwrap()["pending_requests"],
        json!([])
    );
    let output = messages(&mut provider);
    assert!(
        output
            .iter()
            .filter(|(_, message)| message["type"] == "ui_snapshot")
            .all(|(session, _)| *session == 1)
    );

    let stale_metadata = provider.wallet.clone();
    let mut changed_metadata = stale_metadata.clone();
    changed_metadata.default_chain_id = Some(10);
    provider.update_wallet(changed_metadata, 2);
    provider.publish_summaries(
        &stale_metadata,
        2,
        vec![(ready.id.clone(), "Stale metadata summary".into())],
    );
    assert_eq!(
        serde_json::to_value(provider.ui(1)).unwrap()["pending_requests"][0]["summary"],
        "Native intent summary"
    );
    messages(&mut provider);
    provider.update_wallet(GatewayWalletState::default(), 3);
    provider.publish_summaries(
        &provider.wallet.clone(),
        2,
        vec![(ready.id.clone(), "Late secret summary".into())],
    );
    let output = messages(&mut provider);
    assert!(output.iter().any(|(session, message)| *session == 1
        && message["type"] == "ui_snapshot"
        && message["pending_requests"] == json!([])));
    assert!(ready.control.ensure_current().is_err());
    provider
        .request(
            1,
            "doc".into(),
            "locked-sign".into(),
            "personal_sign",
            json!([
                "0x1234",
                ready.authorization.as_ref().unwrap().account.address
            ]),
            Instant::now(),
        )
        .unwrap();
    provider.publish_summaries(
        &provider.wallet.clone(),
        3,
        vec![(ready.id.clone(), "Late secret summary".into())],
    );
    let pending = serde_json::to_value(provider.ui(1)).unwrap();
    assert_eq!(pending["pending_requests"].as_array().unwrap().len(), 1);
    assert_eq!(pending["pending_requests"][0]["needs_unlock"], true);
    assert!(pending["pending_requests"][0]["summary"].is_null());
    messages(&mut provider);
    provider.tick(Instant::now() + APPROVAL_WINDOW);
    assert!(
        messages(&mut provider)
            .iter()
            .any(|(_, message)| message["type"] == "ui_snapshot"
                && message["pending_requests"] == json!([]))
    );
    drop(ready);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn native_ui_events_require_live_session_and_immediate_generation_and_page_methods_cannot_emit_them()
 {
    let (path, mut provider, view) = fixture();
    authorize(
        &mut provider,
        &view,
        url::Url::parse("http://127.0.0.1:1").unwrap(),
    );
    let live = Arc::new(AtomicBool::new(true));
    let event = provider
        .ui_event(2, GatewayUiEventKind::UserActivity, &live)
        .unwrap();
    assert!(event.is_current(&provider.wallet, 2));
    assert!(!event.is_current(&provider.wallet, 3));
    assert!(
        provider
            .ui_event(1, GatewayUiEventKind::SummonDesktop, &live)
            .is_none()
    );
    for method in [
        "user_activity",
        "summon_desktop",
        "request_wallet_switch",
        "private_view",
        "select_wallet",
        "wallets",
    ] {
        let response = request(&mut provider, 1, "doc", method, method, Instant::now());
        assert_eq!(result(&response)["error"]["code"], 4200);
    }
    live.store(false, Ordering::Release);
    assert!(!event.is_current(&provider.wallet, 2));
    let reconnected = Arc::new(AtomicBool::new(true));
    let event = provider
        .ui_event(2, GatewayUiEventKind::UserActivity, &reconnected)
        .unwrap();
    provider
        .authority_fallback
        .as_ref()
        .unwrap()
        .send_replace(GatewayWalletState::default());
    assert!(!event.is_current(&provider.wallet, 2));
    assert!(
        provider
            .ui_event(2, GatewayUiEventKind::UserActivity, &reconnected)
            .is_none()
    );
    drop(event);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn wrong_wallet_connect_completes_only_its_confirmed_transition_or_explicit_active_account_grant()
 {
    for outcome in [
        "confirm",
        "reject",
        "stale",
        "expiry",
        "disconnect",
        "revoke",
        "unrelated",
        "active-account",
    ] {
        let (path, mut provider, target) = fixture();
        let source = wallet(&provider.store, "gateway-other");
        let account = provider
            .store
            .import_public_account(
                PASSWORD,
                &target,
                "0x0000000000000000000000000000000000000000000000000000000000000001",
                Some("Saved account"),
                false,
            )
            .unwrap();
        let peer = PeerId::from_bytes([3; 16]);
        let origin = RpcOrigin::dapp(
            alloy::hex::encode(peer.to_bytes()),
            "https://switch.invalid/path",
        )
        .unwrap();
        let permission = provider
            .store
            .grant_gateway_permission(&target, &origin, &account.public_account_uuid, 1)
            .unwrap();
        provider.update_wallet(state(&source), 2);
        provider
            .register(1, peer, "doc".into(), origin.web_origin().unwrap().as_str())
            .unwrap();
        messages(&mut provider);
        let now = Instant::now();
        let output = request(
            &mut provider,
            1,
            "doc",
            "connect",
            "eth_requestAccounts",
            now,
        );
        let id = prompt(&output)["request_id"].as_str().unwrap().to_owned();
        assert_eq!(prompt(&output)["wrong_wallet"], true);
        let deadline = provider.pending[0].deadline;
        provider.request_wallet_switch(2, peer, 2, &id, now);
        provider.request_wallet_switch(1, PeerId::from_bytes([4; 16]), 2, &id, now);
        provider.request_wallet_switch(1, peer, 1, &id, now);
        assert!(provider.switch_updates.borrow().is_empty());
        provider.request_wallet_switch(1, peer, 2, &id, now);
        provider.request_wallet_switch(1, peer, 2, &id, now);
        assert_eq!(provider.switch_updates.borrow().len(), 1);
        assert_eq!(provider.pending[0].deadline, deadline);
        let native = provider.switch_updates.borrow()[0].clone();
        assert_eq!(native.target_wallet_uuid, target.wallet_id());
        assert!(native.source_is_current(&provider.wallet, 2));
        if outcome == "reject" {
            provider.reject_wallet_switch(&id);
            assert_eq!(result(&messages(&mut provider))["error"]["code"], 4001);
        } else if outcome == "active-account" {
            let active = provider
                .store
                .list_active_public_accounts_for_session(&source)
                .unwrap()
                .remove(0);
            provider.resolve_connect(1, peer, &id, Some(&active.public_account_uuid), 1, now);
            assert_eq!(
                result(&messages(&mut provider))["result"],
                json!([active.address.to_string()])
            );
            assert_eq!(
                provider.store.list_gateway_permissions(&source).unwrap()[0].public_account_uuid,
                active.public_account_uuid
            );
            assert_eq!(
                provider.wallet.view.as_ref().unwrap().wallet_id(),
                source.wallet_id()
            );
        } else {
            provider.begin_wallet_switch(&id).unwrap();
            assert!(provider.begin_wallet_switch(&id).is_err());
            let transition = GatewayWalletSwitchTransition {
                request_id: id.clone(),
                installed: false,
            };
            let mut installing = GatewayWalletState {
                wallet_switch: Some(transition.clone()),
                ..GatewayWalletState::default()
            };
            match outcome {
                "stale" => {
                    installing.wallet_switch = None;
                }
                "expiry" => {
                    provider.tick(deadline);
                }
                "disconnect" => {
                    provider.unregister(1, "doc");
                }
                "revoke" => {
                    provider.revoke(&permission.permission_id).unwrap();
                }
                _ => {}
            }
            provider.update_wallet(installing, 3);
            let output = messages(&mut provider);
            assert!(
                output
                    .iter()
                    .all(|(_, message)| message["type"] != "provider_response"
                        || message.get("result").is_none())
            );
            let mut installed = state(&target);
            installed.active_wallet_generation = 2;
            installed.wallet_switch =
                (outcome != "unrelated").then_some(GatewayWalletSwitchTransition {
                    installed: true,
                    ..transition
                });
            provider.update_wallet(installed, 4);
            let output = messages(&mut provider);
            if outcome == "confirm" {
                assert_eq!(
                    result(&output)["result"],
                    json!([account.address.to_string()])
                );
                assert!(provider.pending.is_empty());
            } else {
                assert!(
                    output
                        .iter()
                        .all(|(_, message)| message["type"] != "provider_response"
                            || message.get("result").is_none())
                );
            }
        }
        assert!(native.control.ensure_current().is_err());
        if !matches!(outcome, "active-account" | "revoke") {
            assert!(provider.store.list_gateway_permissions(&target).unwrap() == vec![permission]);
        }
        drop(native);
        drop(provider);
        drop(source);
        drop(target);
        std::fs::remove_dir_all(path).unwrap();
    }
}
