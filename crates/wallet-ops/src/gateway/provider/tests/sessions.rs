use super::*;

#[test]
fn locked_same_generation_refresh_restores_document_state() {
    let (path, mut provider, view) = fixture();
    provider.update_wallet(GatewayWalletState::default(), 2);
    provider
        .register(
            1,
            PeerId::from_bytes([1; 16]),
            "a".to_owned(),
            "https://example.invalid/",
        )
        .unwrap();
    messages(&mut provider);
    provider.update_wallet(GatewayWalletState::default(), 2);
    assert!(
        messages(&mut provider)
            .iter()
            .any(|(_, message)| message["type"] == "provider_state" && message["document"] == "a")
    );
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn connect_is_owned_by_session_and_web_origin_and_revocation_is_origin_local() {
    let (path, mut provider, view) = fixture();
    let peer = PeerId::from_bytes([1; 16]);
    let now = Instant::now();
    for (session, document, url) in [
        (1, "a", "https://example.invalid/path?q=one#fragment"),
        (1, "b", "https://example.invalid/other?q=two#other"),
        (1, "other", "https://other.invalid/"),
        (2, "a", "https://example.invalid/path?q=one#fragment"),
    ] {
        provider
            .register(session, peer, document.to_owned(), url)
            .unwrap();
    }
    messages(&mut provider);
    let pending = request(&mut provider, 1, "a", "connect", "eth_requestAccounts", now);
    assert!(pending.iter().all(|(session, _)| *session == 1));
    let prompt = prompt(&pending);
    assert_eq!(prompt["url"], "https://example.invalid/");
    assert_eq!(
        prompt["paired_peer_id"],
        alloy::hex::encode(peer.to_bytes())
    );
    let approval = prompt["request_id"].as_str().unwrap().to_owned();
    let account = prompt["accounts"][0]["uuid"].as_str().unwrap().to_owned();
    let address = prompt["accounts"][0]["address"].clone();
    provider.resolve_connect(2, peer, &approval, Some(&account), 1, now);
    provider.resolve_connect(
        1,
        PeerId::from_bytes([2; 16]),
        &approval,
        Some(&account),
        1,
        now,
    );
    assert_eq!(messages(&mut provider), [] as [(u64, Value); 0]);
    assert!(
        provider
            .store
            .list_gateway_permissions(&view)
            .unwrap()
            .is_empty()
    );
    provider.resolve_connect(1, peer, &approval, Some(&account), 1, now);
    let connected = messages(&mut provider);
    let state_index = connected
        .iter()
        .position(|(session, message)| {
            *session == 1 && message["type"] == "provider_state" && message["document"] == "a"
        })
        .unwrap();
    let response_index = connected
        .iter()
        .position(|(_, message)| message["type"] == "provider_response")
        .unwrap();
    assert!(state_index < response_index);
    assert_eq!(result(&connected)["result"], json!([address]));
    assert_eq!(
        result(&request(
            &mut provider,
            1,
            "b",
            "b-accounts",
            "eth_accounts",
            now
        ))["result"],
        json!([address])
    );
    assert_eq!(
        result(&request(
            &mut provider,
            2,
            "a",
            "a-accounts",
            "eth_accounts",
            now
        ))["result"],
        json!([address])
    );
    let grant = provider
        .store
        .list_gateway_permissions(&view)
        .unwrap()
        .remove(0);
    provider
        .register(
            3,
            PeerId::from_bytes([2; 16]),
            "peer".to_owned(),
            "https://example.invalid/",
        )
        .unwrap();
    messages(&mut provider);
    for (session, document) in [(1, "other"), (3, "peer")] {
        assert_eq!(
            result(&request(
                &mut provider,
                session,
                document,
                "unapproved",
                "eth_accounts",
                now
            ))["result"],
            json!([])
        );
        let pending = request(
            &mut provider,
            session,
            document,
            "consent",
            "eth_requestAccounts",
            now,
        );
        assert_ne!(
            snapshot(&pending)["pending_connects"]
                .as_array()
                .unwrap()
                .as_slice(),
            [] as [Value; 0]
        );
    }
    let other_generation = provider.documents[&(1, "other".to_owned())].generation;
    provider.revoke(&grant.permission_id).unwrap();
    let revoked = messages(&mut provider);
    assert!(
        revoked
            .iter()
            .filter(|(_, message)| message["type"] == "provider_state")
            .all(
                |(_, message)| (message["document"] == "a" || message["document"] == "b")
                    && message["accounts"] == json!([])
            )
    );
    assert_eq!(
        provider.documents[&(1, "other".to_owned())].generation,
        other_generation
    );
    assert!(
        provider
            .store
            .list_gateway_permissions(&view)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        result(&request(&mut provider, 1, "a", "chain", "eth_chainId", now))["result"],
        "0x1"
    );
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn scoped_grant_pauses_on_wallet_change_global_grant_survives_and_lock_purges() {
    let (path, mut provider, first) = fixture();
    let second = wallet(&provider.store, "gateway-second");
    let scoped = provider
        .store
        .import_public_account(
            PASSWORD,
            &first,
            "0x0000000000000000000000000000000000000000000000000000000000000001",
            Some("Scoped public"),
            false,
        )
        .unwrap();
    let global = provider
        .store
        .import_public_account(
            PASSWORD,
            &first,
            "0x0000000000000000000000000000000000000000000000000000000000000002",
            Some("Global public"),
            true,
        )
        .unwrap();
    let peer = PeerId::from_bytes([3; 16]);
    let now = Instant::now();
    for (document, account) in [("scoped", &scoped), ("global", &global)] {
        let url = format!("https://{document}.invalid/");
        let origin = RpcOrigin::dapp(alloy::hex::encode(peer.to_bytes()), &url).unwrap();
        provider
            .store
            .grant_gateway_permission(&first, &origin, &account.public_account_uuid, 1)
            .unwrap();
        provider
            .register(1, peer, document.to_owned(), &url)
            .unwrap();
    }
    provider.update_wallet(state(&second), 2);
    messages(&mut provider);
    assert_eq!(
        result(&request(
            &mut provider,
            1,
            "scoped",
            "scoped-read",
            "eth_accounts",
            now
        ))["result"],
        json!([])
    );
    assert_eq!(
        result(&request(
            &mut provider,
            1,
            "global",
            "global-read",
            "eth_accounts",
            now
        ))["result"],
        json!([global.address.to_string()])
    );
    let waiting = request(
        &mut provider,
        1,
        "scoped",
        "connect",
        "eth_requestAccounts",
        now,
    );
    assert_eq!(prompt(&waiting)["wrong_wallet"], true);
    assert!(
        !prompt(&waiting)["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|account| account["uuid"] == scoped.public_account_uuid)
    );
    provider.update_wallet(GatewayWalletState::default(), 3);
    let locked = messages(&mut provider);
    assert!(provider.wallet.view.is_none());
    assert!(provider.permissions().is_empty());
    assert_eq!(
        provider.wallet.public_accounts,
        [] as [PublicAccountMetadata; 0]
    );
    let ui = prompt(&locked);
    assert_eq!(ui["needs_unlock"], true);
    assert_eq!(ui["accounts"], json!([]));
    assert_eq!(
        result(&request(
            &mut provider,
            1,
            "global",
            "locked-read",
            "eth_accounts",
            now
        ))["result"],
        json!([])
    );
    provider.update_wallet(state(&first), 4);
    messages(&mut provider);
    assert_eq!(
        result(&request(
            &mut provider,
            1,
            "scoped",
            "restored",
            "eth_accounts",
            now
        ))["result"],
        json!([scoped.address.to_string()])
    );
    provider
        .store
        .delete_imported_public_account(&first, &scoped.public_account_uuid)
        .unwrap();
    assert_eq!(
        result(&request(
            &mut provider,
            1,
            "scoped",
            "deleted",
            "eth_accounts",
            now
        ))["result"],
        json!([])
    );
    drop(provider);
    drop(first);
    drop(second);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn ui_snapshot_lists_active_accounts_of_the_unlocked_wallet_and_clears_them_on_lock() {
    let (path, mut provider, view) = fixture();
    let active = provider
        .store
        .import_public_account(
            PASSWORD,
            &view,
            "0x0000000000000000000000000000000000000000000000000000000000000005",
            Some("Snapshot public"),
            false,
        )
        .unwrap();
    let mut inactive = provider
        .store
        .import_public_account(
            PASSWORD,
            &view,
            "0x0000000000000000000000000000000000000000000000000000000000000006",
            Some("Retired public"),
            false,
        )
        .unwrap();
    inactive.status = PublicAccountStatus::Inactive;
    let mut wallet = state(&view);
    wallet.public_accounts = vec![active.clone(), inactive];
    wallet.public_view = GatewayPublicView {
        selected_account: Some(active.public_account_uuid.clone()),
        selected_chain: Some(10),
        balances: vec![GatewayAccountBalances {
            account_uuid: active.public_account_uuid.clone(),
            total: None,
            assets: Vec::new(),
        }],
        ..GatewayPublicView::default()
    };
    let peer = PeerId::from_bytes([1; 16]);
    let draft = |peer_id: String, id: &str| crate::gateway::GatewayDraftView {
        peer_id,
        draft_id: id.into(),
        request_id: id.into(),
        revision: 2,
        input: crate::gateway::GatewayDraftInput {
            account: active.public_account_uuid.clone(),
            chain_id: 10,
            kind: crate::gateway::GatewayDraftKind::Send,
            asset: "native".into(),
            amount: "1".into(),
            recipient: "recipient.eth".into(),
            address_book_entry: None,
            fee: crate::gateway::GatewayDraftFee::Normal,
            mimic_railway: false,
            approval: crate::gateway::GatewayDraftApproval::Exact,
            max: false,
        }
        .into(),
        status: crate::gateway::GatewayDraftStatus::Attention,
        estimate: None,
        private_progress: None,
        private_options: None,
        gas_quote: None,
        recipients: Vec::new(),
        step_label: String::new(),
        message: String::new(),
        warning: false,
        can_cancel: true,
        can_retry: false,
    };
    let mine = draft(alloy::hex::encode(peer.to_bytes()), "mine");
    wallet.public_view.drafts = vec![mine.clone(), draft("other-peer".into(), "other")];
    wallet.private_view_supported = true;
    wallet.private_view = Some(crate::gateway::GatewayPrivateView {
        selected_wallet: Some(view.wallet_id().to_owned()),
        selected_wallet_choice: Some(view.wallet_id().to_owned()),
        receive_address: Some(view.receive_address().unwrap()),
        wallets: vec![crate::gateway::GatewayPrivateWallet {
            wallet_id: view.wallet_id().to_owned(),
            label: "Private wallet".into(),
            ..Default::default()
        }],
        selected_chain: Some(10),
        total: Some("$123.45".into()),
        ..Default::default()
    });
    provider.attach_ui_peer(1, peer);
    messages(&mut provider);
    provider.update_wallet(wallet.clone(), 2);
    provider.push_ui(1);
    let unlocked = messages(&mut provider);
    let mut expected = wallet.public_view.clone();
    expected.drafts = vec![mine];
    assert_eq!(
        snapshot(&unlocked)["public_view"],
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(snapshot(&unlocked)["private_view_supported"], true);
    assert_eq!(
        snapshot(&unlocked)["private_view"],
        serde_json::to_value(&wallet.private_view).unwrap()
    );
    assert!(serde_json::to_value(provider.ui(999)).unwrap()["private_view"].is_null());
    assert_eq!(
        snapshot(&unlocked)["accounts"],
        json!([{
            "uuid": active.public_account_uuid,
            "label": active.label,
            "address": active.address.to_string(),
        }])
    );
    // A caller retaining presentation when the view closes cannot disclose it.
    wallet.view = None;
    provider.update_wallet(wallet, 3);
    let locked = messages(&mut provider);
    assert_eq!(snapshot(&locked)["accounts"], json!([]));
    assert!(snapshot(&locked)["private_view"].is_null());
    assert_eq!(
        snapshot(&locked)["public_view"],
        serde_json::to_value(GatewayPublicView::default()).unwrap()
    );
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn public_view_commands_preserve_selection_and_scope_permission_edits_to_the_peer() {
    let (path, mut provider, view) = fixture();
    let peer = PeerId::from_bytes([8; 16]);
    let other_peer = PeerId::from_bytes([9; 16]);
    let accounts = provider
        .store
        .list_public_accounts_for_session(&view, false)
        .unwrap();
    let first = accounts[0].clone();
    let second = provider
        .store
        .import_public_account(
            PASSWORD,
            &view,
            "0x0000000000000000000000000000000000000000000000000000000000000007",
            Some("Another account"),
            false,
        )
        .unwrap();
    let mut wallet = state(&view);
    wallet.public_accounts = vec![first.clone(), second.clone()];
    wallet.public_view.selected_account = Some(first.public_account_uuid.clone());
    wallet.public_view.selected_chain = Some(1);
    provider.update_wallet(wallet, 2);
    provider
        .register(1, peer, "doc".to_owned(), "https://example.invalid/")
        .unwrap();
    provider.attach_ui_peer(2, other_peer);
    messages(&mut provider);

    provider.public_command(
        1,
        peer,
        2,
        GatewayPublicCommand::ConnectTab {
            document: "doc".to_owned(),
        },
    );
    let pending = messages(&mut provider);
    let approval = prompt(&pending)["request_id"].as_str().unwrap().to_owned();
    provider.resolve_connect(
        1,
        peer,
        &approval,
        Some(&second.public_account_uuid),
        10,
        Instant::now(),
    );
    messages(&mut provider);
    assert_eq!(
        provider.wallet.public_view.selected_account.as_deref(),
        Some(first.public_account_uuid.as_str())
    );
    let permission = provider.permissions()[0].clone();
    assert_eq!(
        serde_json::to_value(provider.ui(2)).unwrap()["permissions"],
        json!([])
    );

    for (caller, generation) in [(other_peer, 2), (peer, 1)] {
        provider.public_command(
            2,
            caller,
            generation,
            GatewayPublicCommand::RevokePermission {
                permission_id: permission.permission_id.clone(),
            },
        );
        assert_eq!(provider.permissions().len(), 1);
    }
    provider.public_command(
        1,
        peer,
        2,
        GatewayPublicCommand::ReissuePermission {
            permission_id: permission.permission_id,
            public_account_uuid: first.public_account_uuid.clone(),
        },
    );
    let updated = messages(&mut provider);
    assert!(
        updated
            .iter()
            .any(|(_, value)| value["type"] == "provider_state"
                && value["accounts"] == json!([first.address.to_string()]))
    );
    assert_eq!(provider.permissions()[0].chain_id, 10);
    assert_eq!(
        provider.store.list_gateway_permissions(&view).unwrap()[0].public_account_uuid,
        first.public_account_uuid
    );

    assert!(
        provider
            .public_command(
                1,
                peer,
                1,
                GatewayPublicCommand::SelectAccount {
                    public_account_uuid: second.public_account_uuid.clone()
                }
            )
            .is_none()
    );
    assert!(matches!(
        provider.public_command(
            1,
            peer,
            2,
            GatewayPublicCommand::SelectAccount {
                public_account_uuid: second.public_account_uuid
            }
        ),
        Some(GatewayPublicCommand::SelectAccount { .. })
    ));
    assert_eq!(provider.permissions()[0].chain_id, 10);
    let submit = || GatewayPublicCommand::Draft {
        command: Box::new(crate::gateway::GatewayDraftCommand::Submit {
            draft_id: "draft".into(),
            revision: 2,
        }),
    };
    assert!(provider.public_command(1, peer, 1, submit()).is_none());
    assert!(provider.public_command(1, peer, 2, submit()).is_some());
    let private_create = || GatewayPublicCommand::Draft {
        command: Box::new(crate::gateway::GatewayDraftCommand::Create {
            request_id: "private".into(),
            input: crate::gateway::GatewayDraftPayload::Private(
                crate::gateway::GatewayPrivateDraftInput::default(),
            ),
        }),
    };
    provider.attach_ui_peer(1, peer);
    assert!(
        provider
            .public_command(1, peer, 2, private_create())
            .is_none()
    );
    let mut supported = provider.wallet.clone();
    supported.private_actions_supported = true;
    provider.update_wallet(supported, 2);
    assert!(
        provider
            .public_command(1, peer, 1, private_create())
            .is_none()
    );
    assert!(
        provider
            .public_command(1, other_peer, 2, private_create())
            .is_none()
    );
    assert!(
        provider
            .public_command(2, peer, 2, private_create())
            .is_none()
    );
    assert!(
        provider
            .public_command(1, peer, 2, private_create())
            .is_some()
    );
    let self_create = || {
        let input = crate::gateway::GatewayPrivateDraftInput {
            delivery: crate::gateway::GatewayPrivateDelivery::SelfBroadcast {
                delivery: crate::gateway::GatewayPrivateSelfBroadcastInput::SelfBroadcast {
                    signer: None,
                    funding: crate::gateway::GatewayPrivateFunding::PublicBalance {},
                    fee: crate::gateway::GatewayPrivateGasFee::Auto {},
                },
            },
            ..Default::default()
        };
        GatewayPublicCommand::Draft {
            command: Box::new(crate::gateway::GatewayDraftCommand::Create {
                request_id: "self".into(),
                input: crate::gateway::GatewayDraftPayload::Private(input),
            }),
        }
    };
    assert!(provider.public_command(1, peer, 2, self_create()).is_none());
    let mut supported = provider.wallet.clone();
    supported.private_self_broadcast_supported = true;
    provider.update_wallet(supported, 2);
    assert!(provider.public_command(1, peer, 2, self_create()).is_some());
    assert!(provider.public_command(1, peer, 1, self_create()).is_none());
    assert!(
        provider
            .public_command(1, other_peer, 2, self_create())
            .is_none()
    );
    assert!(provider.public_command(2, peer, 2, self_create()).is_none());
    let mut unsupported = provider.wallet.clone();
    unsupported.private_self_broadcast_supported = false;
    provider
        .authority_fallback
        .as_ref()
        .unwrap()
        .send_replace(unsupported);
    assert!(provider.public_command(1, peer, 2, self_create()).is_none());
    assert!(
        provider
            .public_command(1, peer, 2, private_create())
            .is_some()
    );
    let mut unsupported = provider.wallet.clone();
    unsupported.private_actions_supported = false;
    provider
        .authority_fallback
        .as_ref()
        .unwrap()
        .send_replace(unsupported);
    assert!(
        provider
            .public_command(1, peer, 2, private_create())
            .is_none()
    );
    invalidate_authority(&provider, true);
    assert!(provider.public_command(1, peer, 2, submit()).is_none());
    assert!(
        provider
            .public_command(
                1,
                peer,
                2,
                GatewayPublicCommand::SelectChain { chain_id: 10 }
            )
            .is_none()
    );
    provider.update_wallet(provider.wallet.clone(), 2);
    provider.public_command(
        1,
        peer,
        2,
        GatewayPublicCommand::RevokePermission {
            permission_id: provider.permissions()[0].permission_id.clone(),
        },
    );
    let revoked = messages(&mut provider);
    assert!(
        revoked
            .iter()
            .any(|(_, value)| value["type"] == "provider_state" && value["accounts"] == json!([]))
    );
    assert!(
        provider
            .store
            .list_gateway_permissions(&view)
            .unwrap()
            .is_empty()
    );
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test(start_paused = true)]
async fn approval_window_includes_unlock_wait_and_rate_budget_survives_disconnect() {
    let (path, mut provider, view) = fixture();
    let peer = PeerId::from_bytes([4; 16]);
    let now = Instant::now();
    provider.update_wallet(GatewayWalletState::default(), 2);
    provider
        .register(1, peer, "doc".to_owned(), "https://example.invalid/")
        .unwrap();
    messages(&mut provider);
    for id in 0..16 {
        let waiting = request(
            &mut provider,
            1,
            "doc",
            &format!("approval-{id}"),
            "eth_requestAccounts",
            now,
        );
        assert_eq!(prompt(&waiting)["needs_unlock"], true);
        assert_eq!(prompt(&waiting)["accounts"], json!([]));
    }
    assert_eq!(
        result(&request(
            &mut provider,
            1,
            "doc",
            "overflow",
            "eth_requestAccounts",
            now
        ))["error"]["code"],
        -32005
    );
    provider.update_wallet(state(&view), 3);
    messages(&mut provider);
    tokio::time::advance(APPROVAL_WINDOW).await;
    let now = Instant::now();
    provider.tick(now);
    let expired = messages(&mut provider);
    assert_eq!(
        expired
            .iter()
            .filter(|(_, message)| message["error"]["code"] == -32002)
            .count(),
        16
    );
    assert!(provider.pending.is_empty());
    for id in 0..64 {
        assert_eq!(
            result(&request(
                &mut provider,
                1,
                "doc",
                &format!("read-{id}"),
                "eth_accounts",
                now
            ))["result"],
            json!([])
        );
    }
    provider.retire_sessions(|_| false);
    provider
        .register(2, peer, "new".to_owned(), "https://example.invalid/")
        .unwrap();
    messages(&mut provider);
    assert_eq!(
        result(&request(
            &mut provider,
            2,
            "new",
            "rate",
            "eth_accounts",
            now
        ))["error"]["code"],
        -32005
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        result(&request(
            &mut provider,
            2,
            "new",
            "refill",
            "eth_accounts",
            Instant::now()
        ))["result"],
        json!([])
    );
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}
