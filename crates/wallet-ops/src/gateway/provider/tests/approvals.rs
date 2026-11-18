use super::*;

#[tokio::test]
async fn locked_connect_and_sign_share_capacity_and_original_expiry() {
    let (path, mut provider, view) = fixture();
    authorize(
        &mut provider,
        &view,
        url::Url::parse("http://127.0.0.1:1").unwrap(),
    );
    let mut wallet = provider.wallet.clone();
    let account = provider
        .resolve(&provider.documents[&(1, "doc".to_owned())].origin)
        .unwrap()
        .1
        .address;
    provider.update_wallet(GatewayWalletState::default(), 3);
    messages(&mut provider);
    let now = Instant::now();
    for index in 0..16 {
        let (method, params) = if index % 2 == 0 {
            ("eth_requestAccounts", json!([]))
        } else {
            ("personal_sign", json!(["0x1234", account]))
        };
        provider
            .request(
                1,
                "doc".into(),
                format!("pending-{index}"),
                method,
                params,
                now,
            )
            .unwrap();
    }
    assert!(provider.approval_updates.borrow().is_empty());
    assert!(provider.reads.is_empty());
    let rejected = request(
        &mut provider,
        1,
        "doc",
        "overflow",
        "eth_requestAccounts",
        now,
    );
    assert_eq!(result(&rejected)["error"]["code"], -32005);
    // Password metadata and software/hardware preparation can span arbitrary generations.
    provider.update_wallet(
        GatewayWalletState {
            active_wallet_generation: 17,
            ..GatewayWalletState::default()
        },
        17,
    );
    assert!(provider.approval_updates.borrow().is_empty());
    wallet.active_wallet_generation = 42;
    wallet.waiting_unlock.completed = true;
    provider.update_wallet(wallet, 42);
    assert_eq!(provider.approval_updates.borrow().len(), 8);
    assert!(
        provider
            .approval_updates
            .borrow()
            .iter()
            .all(|request| request.deadline == now + APPROVAL_WINDOW)
    );
    messages(&mut provider);
    provider.tick(now + APPROVAL_WINDOW);
    let expired = messages(&mut provider);
    assert_eq!(
        expired
            .iter()
            .filter(|(_, value)| value["error"]["code"] == -32002)
            .count(),
        16
    );
    assert!(provider.approval_updates.borrow().is_empty());
    provider.update_wallet(GatewayWalletState::default(), 5);
    provider
        .request(
            1,
            "doc".into(),
            "locked-expiry".into(),
            "personal_sign",
            json!(["0x1234", account]),
            now,
        )
        .unwrap();
    provider.tick(now + APPROVAL_WINDOW);
    assert_eq!(result(&messages(&mut provider))["error"]["code"], 4100);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn locked_approvals_bound_params_size_and_total_across_origins() {
    let (path, mut provider, view) = fixture();
    provider.update_wallet(GatewayWalletState::default(), 2);
    for index in 0..=MAX_APPROVALS {
        provider
            .register(
                1,
                PeerId::from_bytes([1; 16]),
                format!("doc-{index}"),
                &format!("https://site-{index}.invalid/"),
            )
            .unwrap();
    }
    messages(&mut provider);
    let now = Instant::now();
    let sign = |provider: &mut DappProvider, index: usize, message: String| {
        provider
            .request(
                1,
                format!("doc-{index}"),
                "sign".into(),
                "personal_sign",
                json!([message, "0x0000000000000000000000000000000000000001"]),
                now,
            )
            .unwrap();
        messages(provider)
    };
    let oversized = sign(&mut provider, 0, format!("0x{}", "00".repeat(1024 * 1024)));
    assert_eq!(result(&oversized)["error"]["code"], -32005);
    for index in 0..MAX_APPROVALS {
        let admitted = sign(&mut provider, index, "0x1234".into());
        assert!(
            admitted
                .iter()
                .all(|(_, m)| m["type"] != "provider_response")
        );
    }
    let overflow = sign(&mut provider, MAX_APPROVALS, "0x1234".into());
    assert_eq!(result(&overflow)["error"]["code"], -32005);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn waiting_approvals_reject_retired_cohorts_and_stale_admission() {
    for coalesced in [false, true] {
        let (path, mut provider, view) = fixture();
        authorize(
            &mut provider,
            &view,
            url::Url::parse("http://127.0.0.1:1").unwrap(),
        );
        let mut unlocked = provider.wallet.clone();
        let account = provider
            .resolve(&provider.documents[&(1, "doc".to_owned())].origin)
            .unwrap()
            .1
            .address;
        provider.update_wallet(GatewayWalletState::default(), 3);
        messages(&mut provider);
        let now = Instant::now();
        provider
            .request(
                1,
                "doc".into(),
                "old".into(),
                "personal_sign",
                json!(["0x1234", account]),
                now,
            )
            .unwrap();
        assert!(provider.approval_updates.borrow().is_empty());
        let authority = provider.authority_fallback.take().unwrap();
        if coalesced {
            unlocked.waiting_unlock.completed = true;
            authority.send_replace(unlocked.clone());
        }
        // A locked wallet/settings change or unlock/relock retires the first cohort immediately.
        let locked = GatewayWalletState {
            waiting_unlock: GatewayUnlockState {
                cohort: 1,
                completed: false,
            },
            ..GatewayWalletState::default()
        };
        authority.send_replace(locked.clone());
        provider
            .request(
                1,
                "doc".into(),
                "stale-admission".into(),
                "personal_sign",
                json!(["0x1234", account]),
                now,
            )
            .unwrap();
        let rejected = messages(&mut provider);
        assert!(
            rejected
                .iter()
                .any(|(_, value)| value["request_id"] == "stale-admission"
                    && value["error"]["code"] == 4100)
        );
        assert!(provider.approval_updates.borrow().is_empty());
        provider.update_wallet(locked, 5);
        messages(&mut provider);
        assert!(provider.approvals.is_empty());
        provider
            .request(
                1,
                "doc".into(),
                "fresh".into(),
                "personal_sign",
                json!(["0x1234", account]),
                now,
            )
            .unwrap();
        unlocked.waiting_unlock = GatewayUnlockState {
            cohort: 1,
            completed: true,
        };
        authority.send_replace(unlocked.clone());
        provider.update_wallet(unlocked, 6);
        let ready = provider.approval_updates.borrow().clone();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].deadline, now + APPROVAL_WINDOW);
        assert_eq!(provider.approvals[0].request_id, "fresh");
        assert!(provider.begin_approval(&ready[0].id).is_ok());
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn waiting_approval_requires_explicit_usable_unlock_completion() {
    let (path, mut provider, view) = fixture();
    authorize(
        &mut provider,
        &view,
        url::Url::parse("http://127.0.0.1:1").unwrap(),
    );
    let unlocked = provider.wallet.clone();
    let account = provider
        .resolve(&provider.documents[&(1, "doc".to_owned())].origin)
        .unwrap()
        .1
        .address;
    provider.update_wallet(GatewayWalletState::default(), 3);
    messages(&mut provider);
    provider
        .request(
            1,
            "doc".into(),
            "unrelated-install".into(),
            "personal_sign",
            json!(["0x1234", account]),
            Instant::now(),
        )
        .unwrap();
    provider.update_wallet(unlocked, 4);
    assert!(provider.approval_updates.borrow().is_empty());
    assert!(provider.approvals.is_empty());
    assert_eq!(result(&messages(&mut provider))["error"]["code"], 4100);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn hardware_session_refresh_preserves_approval_and_signature_delivery() {
    use crate::hardware::HardwareDeviceKind;
    use crate::vault::{HardwareProfileBinding, HardwareProfileSession};

    let (path, mut provider, view) = fixture();
    let mut profile = HardwareProfileSession::matched(
        HardwareDeviceKind::Trezor,
        "gateway-profile",
        HardwareProfileBinding::evm_address_fingerprint("gateway-binding"),
        Some(vec![1, 2, 3]),
    );
    let view = Arc::new(view.clone_with_hardware_profile_session(profile.clone()));
    authorize(
        &mut provider,
        &view,
        url::Url::parse("http://127.0.0.1:1").unwrap(),
    );
    personal_approval(&mut provider, "sign", Instant::now());
    let ready = provider.approval_updates.borrow()[0].clone();
    provider.begin_approval(&ready.id).unwrap();

    profile.trezor_session_id = Some(vec![4, 5, 6]);
    let refreshed = Arc::new(view.clone_with_hardware_profile_session(profile.clone()));
    let mut wallet = provider.wallet.clone();
    wallet.view = Some(Arc::clone(&refreshed));
    provider.update_wallet(wallet, provider.generation);
    assert!(ready.control.ensure_current().is_ok());
    provider
        .complete_approval(&ready.id, Ok(json!("synthetic-signature")))
        .unwrap();
    let mut delivery = provider
        .drain()
        .into_iter()
        .find(|(_, delivery)| {
            matches!(
                delivery.message,
                GatewayServerMessage::ProviderResponse { .. }
            )
        })
        .unwrap()
        .1;

    profile.trezor_session_id = Some(vec![7, 8, 9]);
    let mut wallet = provider.wallet.clone();
    wallet.view = Some(Arc::new(
        refreshed.clone_with_hardware_profile_session(profile.clone()),
    ));
    provider.update_wallet(wallet, provider.generation);
    assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Current);
    assert_eq!(
        serde_json::to_value(&delivery.message).unwrap()["result"],
        "synthetic-signature"
    );

    personal_approval(&mut provider, "other-sign", Instant::now());
    let pending = provider.approval_updates.borrow()[0].clone();
    profile.binding = HardwareProfileBinding::evm_address_fingerprint("other-binding");
    let mut wallet = provider.wallet.clone();
    wallet.view = Some(Arc::new(
        refreshed.clone_with_hardware_profile_session(profile),
    ));
    provider.update_wallet(wallet, provider.generation);
    assert!(pending.control.ensure_current().is_err());
    assert!(provider.approval_updates.borrow().is_empty());
    assert!(provider.delivery(1, &mut delivery) != DeliveryStatus::Current);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn native_approval_decisions_and_delivery_remain_bound_to_original_authority() {
    for invalidation in [
        "document",
        "revoke",
        "wallet",
        "reopen",
        "network",
        "immediate",
    ] {
        let (path, mut provider, view) = fixture();
        authorize(
            &mut provider,
            &view,
            url::Url::parse("http://127.0.0.1:1").unwrap(),
        );
        personal_approval(&mut provider, "sign", Instant::now());
        let ready = provider.approval_updates.borrow()[0].clone();
        assert_ne!(ready.id, "sign");
        assert!(provider.begin_approval(&ready.id).is_ok());
        assert!(provider.begin_approval(&ready.id).is_err());
        provider.return_approval_to_review(&ready.id).unwrap();
        assert!(provider.begin_approval(&ready.id).is_ok());
        provider
            .complete_approval(&ready.id, Ok(json!("synthetic-signature")))
            .unwrap();
        let mut delivery = provider
            .drain()
            .into_iter()
            .find(|(_, delivery)| {
                matches!(
                    delivery.message,
                    GatewayServerMessage::ProviderResponse { .. }
                )
            })
            .unwrap()
            .1;
        assert!(delivery.ticket_id().is_none());
        assert_eq!(delivery.deadline(), Some(ready.deadline));
        personal_approval(&mut provider, "other-sign", Instant::now());
        let pending = provider.approval_updates.borrow()[0].clone();
        match invalidation {
            "document" => provider.unregister(1, "doc"),
            "revoke" => {
                provider
                    .revoke(&provider.permissions[0].permission_id.clone())
                    .unwrap();
            }
            "wallet" => {
                let mut wallet = provider.wallet.clone();
                wallet.active_wallet_generation += 1;
                provider.update_wallet(wallet, 3);
            }
            "reopen" => {
                let mut wallet = provider.wallet.clone();
                wallet.view = Some(Arc::new(
                    provider
                        .store
                        .load_view_session(PASSWORD, view.wallet_id())
                        .unwrap(),
                ));
                provider.update_wallet(wallet, provider.generation);
            }
            "network" => {
                let mut wallet = provider.wallet.clone();
                wallet.http = Some(HttpContext::direct_for_tests());
                provider.update_wallet(wallet, 3);
            }
            "immediate" => {
                provider
                    .authority_fallback
                    .as_ref()
                    .unwrap()
                    .send_replace(GatewayWalletState::default());
                assert_eq!(
                    pending.control.ensure_current(),
                    Err(RpcBrokerError::OriginRejected)
                );
                provider.tick(Instant::now());
            }
            _ => unreachable!(),
        }
        assert!(pending.control.ensure_current().is_err());
        assert!(provider.approval_updates.borrow().is_empty());
        assert!(provider.delivery(1, &mut delivery) != DeliveryStatus::Current);
        assert!(
            serde_json::from_value::<crate::gateway::GatewayClientMessage>(
                json!({"type":"resolve_sign", "version":1, "request_id":ready.id})
            )
            .is_err()
        );
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn native_review_reads_preserve_dapp_budget_and_drain_after_retirement() {
    let (path, mut provider, view) = fixture();
    let (endpoint, started, release, server) = held_rpc(false).await;
    authorize(&mut provider, &view, endpoint.clone());
    personal_approval(&mut provider, "sign", Instant::now());
    let ready = provider.approval_updates.borrow()[0].clone();
    let rpc = || RpcRead::from_method_params("eth_blockNumber", json!([]), 1).unwrap();
    let (reply, rejected) = oneshot::channel();
    provider.approval_read(
        &ready.id,
        url::Url::parse("http://127.0.0.1:2").unwrap().into(),
        rpc(),
        Instant::now(),
        operation_client(),
        reply,
    );
    assert_eq!(rejected.await.unwrap(), Err(RpcBrokerError::OriginRejected));
    let (reply, response) = oneshot::channel();
    provider.approval_read(
        &ready.id,
        endpoint.clone().into(),
        rpc(),
        Instant::now(),
        operation_client(),
        reply,
    );
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let now = Instant::now();
    for _ in 0..32 {
        assert!(matches!(
            provider.admission.admit(ready.origin.clone(), now),
            Ok(ReadAdmissionDecision::Ready(_))
        ));
    }
    for _ in 0..32 {
        assert!(matches!(
            provider.admission.admit(ready.origin.clone(), now),
            Ok(ReadAdmissionDecision::Queued(_))
        ));
    }
    assert!(provider.admission.admit(ready.origin.clone(), now).is_err());
    provider.unregister(1, "doc");
    assert_eq!(response.await.unwrap(), Err(RpcBrokerError::Shutdown));
    assert_eq!(provider.reads.len(), 1);
    assert!(
        provider
            .reads
            .values()
            .all(|read| read.retired && read.phase == ReadPhase::Running)
    );
    release.notify_one();
    let completion = provider.jobs.join_next().await.unwrap().unwrap();
    provider.complete_read(completion);
    server.await.unwrap();
    assert!(provider.reads.is_empty());
    assert!(
        messages(&mut provider)
            .iter()
            .all(|(_, value)| value["result"] != "0xfeed")
    );
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}
