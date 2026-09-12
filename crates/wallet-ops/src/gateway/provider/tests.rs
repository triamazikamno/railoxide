use super::*;
use crate::gateway::{GatewayAccountBalances, GatewayPublicCommand, GatewayPublicView};
use crate::vault::{KdfParams, PublicAccountStatus, WalletSource};
use local_db::{DbConfig, DbStore};

mod private_view;

const PASSWORD: &str = "gateway synthetic test password";

pub(in crate::gateway) fn wallet(store: &DesktopVaultStore, id: &str) -> Arc<DesktopViewSession> {
    let metadata = store
        .new_wallet_metadata(PASSWORD, id, 0, WalletSource::Imported, id)
        .unwrap();
    store.import_wallet_mnemonic_with_metadata(PASSWORD, id, 0, "english", "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about", &metadata).unwrap();
    Arc::new(store.load_view_session(PASSWORD, id).unwrap())
}
pub(in crate::gateway) fn initialize(store: &DesktopVaultStore) -> Arc<DesktopViewSession> {
    store
        .create_vault_with_params(PASSWORD, KdfParams::new(1024, 1, 1))
        .unwrap();
    wallet(store, "gateway-first")
}
pub(in crate::gateway) fn fixture() -> (std::path::PathBuf, DappProvider, Arc<DesktopViewSession>) {
    let mut id = [0; 16];
    getrandom::fill(&mut id).unwrap();
    let path = std::env::temp_dir().join(format!("gateway-provider-{}", alloy::hex::encode(id)));
    let store = DesktopVaultStore::from_db(Arc::new(
        DbStore::open(DbConfig {
            root_dir: path.clone(),
        })
        .unwrap(),
    ));
    let view = initialize(&store);
    let mut provider = DappProvider::new(store, 0);
    provider.update_wallet(state(&view), 1);
    (path, provider, view)
}
pub(in crate::gateway) fn invalidate_authority(provider: &DappProvider, locked: bool) {
    let mut wallet = if locked {
        GatewayWalletState::default()
    } else {
        provider.wallet.clone()
    };
    wallet.active_wallet_generation += 1;
    provider
        .authority_fallback
        .as_ref()
        .unwrap()
        .send_replace(wallet);
}

pub(in crate::gateway) fn state(view: &Arc<DesktopViewSession>) -> GatewayWalletState {
    GatewayWalletState {
        view: Some(Arc::clone(view)),
        active_wallet_generation: 1,
        public_accounts: Vec::new(),
        chain_ids: vec![1, 10],
        default_chain_id: Some(1),
        ..GatewayWalletState::default()
    }
}
fn messages(provider: &mut DappProvider) -> Vec<(u64, Value)> {
    let mut output = Vec::new();
    loop {
        let messages = provider.drain();
        if messages.is_empty() {
            break;
        }
        for (session, mut message) in messages {
            let status = provider.delivery(session, &mut message);
            let ticket = message.ticket_id();
            if status != DeliveryStatus::Discard {
                output.push((session, serde_json::to_value(message.message).unwrap()));
            }
            if let Some(id) = ticket {
                provider.delivered(id);
            }
        }
    }
    output
}
fn request(
    provider: &mut DappProvider,
    session: u64,
    document: &str,
    request_id: &str,
    method: &str,
    now: Instant,
) -> Vec<(u64, Value)> {
    provider
        .request(
            session,
            document.to_owned(),
            request_id.to_owned(),
            method,
            json!([]),
            now,
        )
        .unwrap();
    messages(provider)
}
fn result(messages: &[(u64, Value)]) -> &Value {
    &messages
        .iter()
        .find(|(_, message)| message["type"] == "provider_response")
        .unwrap()
        .1
}
fn snapshot(messages: &[(u64, Value)]) -> &Value {
    &messages
        .iter()
        .find(|(_, message)| message["type"] == "ui_snapshot")
        .unwrap()
        .1
}
fn prompt(messages: &[(u64, Value)]) -> &Value {
    &snapshot(messages)["pending_connects"][0]
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
    assert!(messages(&mut provider).is_empty());
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
        assert!(
            !snapshot(&pending)["pending_connects"]
                .as_array()
                .unwrap()
                .is_empty()
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
        result(&request(&mut provider, 1, "a", "chain", "eth_chainId", now))["error"]["code"],
        4100
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
    assert!(provider.wallet.public_accounts.is_empty());
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

/// One synthetic HTTP exchange, held after request ingress and before its response.
pub(in crate::gateway) async fn held_rpc(
    remote_error: bool,
) -> (
    url::Url,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<()>,
) {
    held_responses(
        remote_error,
        BTreeMap::from([("eth_blockNumber", json!("0xfeed"))]),
    )
    .await
}

async fn held_responses(
    remote_error: bool,
    mut responses: BTreeMap<&'static str, Value>,
) -> (
    url::Url,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let task = tokio::spawn({
        let started = started.clone();
        let release = release.clone();
        async move {
            let mut held = false;
            while !responses.is_empty() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let request = loop {
                    let mut chunk = [0; 4096];
                    let length = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(length, 0);
                    bytes.extend_from_slice(&chunk[..length]);
                    if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&bytes[..index]).unwrap();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        if bytes.len() >= index + 4 + length {
                            break serde_json::from_slice::<Value>(
                                &bytes[index + 4..index + 4 + length],
                            )
                            .unwrap();
                        }
                    }
                };
                let result = responses
                    .remove(request["method"].as_str().unwrap())
                    .expect("expected RPC method");
                if !held {
                    started.notify_one();
                    release.notified().await;
                    held = true;
                }
                let response = if remote_error {
                    json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":-32000,"message":"synthetic-owner-payload","data":{"private":"synthetic-owner-payload"},"extension":[null,1]}})
                } else {
                    json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
                };
                let body = serde_json::to_vec(&response).unwrap();
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        }
    });
    (endpoint, started, release, task)
}

pub(in crate::gateway) fn authorize(
    provider: &mut DappProvider,
    view: &Arc<DesktopViewSession>,
    endpoint: url::Url,
) -> (PeerId, GatewayPermission) {
    let peer = PeerId::from_bytes([8; 16]);
    let origin = RpcOrigin::dapp(
        alloy::hex::encode(peer.to_bytes()),
        "https://reads.invalid/path?q=1#doc",
    )
    .unwrap();
    let account = provider
        .store
        .list_active_public_accounts_for_session(view)
        .unwrap()
        .remove(0);
    provider
        .store
        .grant_gateway_permission(view, &origin, &account.public_account_uuid, 1)
        .unwrap();
    let mut wallet = state(view);
    wallet.http = Some(HttpContext::direct_for_tests());
    wallet
        .routes
        .insert(1, RpcChainRoute::new(1, vec![endpoint]));
    provider.update_wallet(wallet, 2);
    provider
        .register(
            1,
            peer,
            "doc".to_owned(),
            origin.web_origin().unwrap().as_str(),
        )
        .unwrap();
    messages(provider);
    (peer, provider.permissions[0].clone())
}

#[tokio::test]
async fn accepted_reads_drain_after_lock_revoke_and_document_retirement_without_old_payloads() {
    for remote_error in [false, true] {
        for invalidation in ["lock", "authority", "revoke", "disconnect", "network"] {
            let (path, mut provider, view) = fixture();
            let (endpoint, started, release, server) = held_rpc(remote_error).await;
            let (peer, permission) = authorize(&mut provider, &view, endpoint);
            let now = Instant::now();
            assert!(request(&mut provider, 1, "doc", "delayed", "eth_blockNumber", now).is_empty());
            tokio::time::timeout(Duration::from_secs(5), started.notified())
                .await
                .unwrap();
            let id = *provider.reads.keys().next().unwrap();
            match invalidation {
                "lock" => provider.update_wallet(GatewayWalletState::default(), 3),
                "authority" => {
                    invalidate_authority(&provider, true);
                    provider.tick(Instant::now());
                }
                "revoke" => provider.revoke(&permission.permission_id).unwrap(),
                "network" => {
                    let mut wallet = provider.wallet.clone();
                    wallet.http = Some(HttpContext::direct_for_tests());
                    provider.update_wallet(wallet, 3);
                }
                _ => provider.retire_sessions(|_| false),
            }
            let retired = messages(&mut provider);
            if invalidation != "disconnect" {
                assert_eq!(
                    result(&retired)["error"]["code"],
                    if invalidation == "network" {
                        -32002
                    } else {
                        4100
                    }
                );
            }
            assert!(provider.reads[&id].retired);
            assert!(provider.reads[&id].phase == ReadPhase::Running);
            // Thirty-one other tickets fit. The invalidated broker submission still owns one.
            let mut charged = Vec::new();
            for _ in 0..31 {
                let ReadAdmissionDecision::Ready(ticket) = provider
                    .admission
                    .admit(permission.origin.clone(), Instant::now())
                    .unwrap()
                else {
                    panic!("remaining active slot");
                };
                charged.push(ticket);
            }
            let ReadAdmissionDecision::Queued(queued) = provider
                .admission
                .admit(permission.origin.clone(), Instant::now())
                .unwrap()
            else {
                panic!("retired submission must remain charged");
            };
            provider.admission.cancel_queued(queued.id);
            provider
                .register(
                    2,
                    peer,
                    "new".to_owned(),
                    permission.origin.web_origin().unwrap().as_str(),
                )
                .unwrap();
            messages(&mut provider);
            release.notify_one();
            let completion = provider.jobs.join_next().await.unwrap().unwrap();
            provider.complete_read(completion);
            assert!(messages(&mut provider).is_empty());
            assert!(!provider.reads.contains_key(&id));
            assert!(matches!(
                provider
                    .admission
                    .admit(permission.origin.clone(), Instant::now()),
                Ok(ReadAdmissionDecision::Ready(_))
            ));
            for ticket in charged {
                provider.admission.complete(ticket.id, Instant::now());
            }
            server.await.unwrap();
            drop(provider);
            drop(view);
            std::fs::remove_dir_all(path).unwrap();
        }
    }
}

#[tokio::test]
async fn queued_reads_keep_the_entry_deadline_and_expiry_never_dispatches() {
    for invalidation in ["none", "deadline", "authority"] {
        let (path, mut provider, view) = fixture();
        let (endpoint, started, release, server) = held_rpc(false).await;
        authorize(&mut provider, &view, endpoint);
        let now = Instant::now() - Duration::from_secs(5);
        // Hold actual local response delivery, filling the same admission as remote work.
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
        let id = *provider
            .reads
            .iter()
            .find(|(_, read)| read.owner.request_id == "queued")
            .unwrap()
            .0;
        let deadline = provider.reads[&id].ticket.deadline;
        assert!(provider.reads[&id].phase == ReadPhase::Queued);
        for duplicate in ["local-0", "queued"] {
            assert!(
                provider
                    .request(
                        1,
                        "doc".to_owned(),
                        duplicate.to_owned(),
                        "eth_accounts",
                        json!([]),
                        now
                    )
                    .is_err()
            );
        }
        // Another web origin retains its independent admission while this origin waits.
        provider
            .register(
                2,
                PeerId::from_bytes([8; 16]),
                "other".to_owned(),
                "https://other.invalid/",
            )
            .unwrap();
        provider
            .request(
                2,
                "other".to_owned(),
                "independent".to_owned(),
                "eth_accounts",
                json!([]),
                now,
            )
            .unwrap();
        assert!(provider.reads.values().any(
            |read| read.owner.request_id == "independent" && read.phase == ReadPhase::Delivery
        ));
        if invalidation == "none" {
            messages(&mut provider);
            assert!(provider.reads[&id].phase == ReadPhase::Running);
            assert_eq!(provider.reads[&id].ticket.deadline, deadline);
            started.notified().await;
            release.notify_one();
            let completion = provider.jobs.join_next().await.unwrap().unwrap();
            provider.complete_read(completion);
            let output = messages(&mut provider);
            assert!(
                output
                    .iter()
                    .any(|(_, message)| message["request_id"] == "queued"
                        && message["result"] == "0xfeed")
            );
            server.await.unwrap();
        } else {
            if invalidation == "authority" {
                invalidate_authority(&provider, true);
                provider.tick(Instant::now());
            } else {
                provider.tick(now + Duration::from_secs(30));
            }
            assert!(provider.reads[&id].retired);
            assert!(provider.reads[&id].phase == ReadPhase::Delivery);
            assert!(provider.jobs.is_empty());
            assert!(result(&messages(&mut provider))["error"].is_object());
            server.abort();
        }
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn authorized_completion_preserves_remote_payload_across_default_chain_change() {
    for remote_error in [false, true] {
        let (path, mut provider, view) = fixture();
        let (endpoint, started, release, server) = held_rpc(remote_error).await;
        authorize(&mut provider, &view, endpoint);
        request(
            &mut provider,
            1,
            "doc",
            "read",
            "eth_blockNumber",
            Instant::now(),
        );
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        let mut wallet = provider.wallet.clone();
        wallet.default_chain_id = Some(10);
        assert!(provider.wallet.same_authority(&wallet));
        assert!(!provider.wallet.same_state(&wallet));
        provider.update_wallet(wallet, provider.generation);
        messages(&mut provider);
        release.notify_one();
        let completion = provider.jobs.join_next().await.unwrap().unwrap();
        provider.complete_read(completion);
        let output = messages(&mut provider);
        if remote_error {
            assert_eq!(
                result(&output)["error"],
                json!({"code":-32000,"message":"synthetic-owner-payload","data":{"private":"synthetic-owner-payload"},"extension":[null,1]})
            );
        } else {
            assert_eq!(result(&output)["result"], "0xfeed");
        }
        assert!(provider.reads.is_empty());
        server.await.unwrap();
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn terminal_delivery_retains_document_ownership_when_broker_finishes_first() {
    for drain_first in [false, true] {
        for unregister in [false, true] {
            let (path, mut provider, view) = fixture();
            let (endpoint, started, release, server) = held_rpc(false).await;
            let (_, permission) = authorize(&mut provider, &view, endpoint);
            request(
                &mut provider,
                1,
                "doc",
                "retired",
                "eth_blockNumber",
                Instant::now(),
            );
            started.notified().await;
            let id = *provider.reads.keys().next().unwrap();
            let deadline = provider.reads[&id].ticket.deadline;
            provider.tick(deadline);
            let mut deliveries = if drain_first {
                provider.drain()
            } else {
                Vec::new()
            };
            release.notify_one();
            let completion = provider.jobs.join_next().await.unwrap().unwrap();
            provider.complete_read(completion);
            if !drain_first {
                deliveries = provider.drain();
            }
            // The old broker job has finished, so all 32 slots are available before delivery.
            let charged: Vec<_> = (0..32)
                .map(|_| {
                    let ReadAdmissionDecision::Ready(ticket) = provider
                        .admission
                        .admit(permission.origin.clone(), Instant::now())
                        .unwrap()
                    else {
                        panic!("completed job must release its active slot");
                    };
                    ticket
                })
                .collect();
            if unregister {
                provider.unregister(1, "doc");
            }
            let (_, mut delivery) = deliveries
                .into_iter()
                .find(|(_, delivery)| delivery.ticket_id() == Some(id))
                .unwrap();
            assert_eq!(delivery.read.as_ref().unwrap().0.deadline, deadline);
            assert!(delivery.deadline().is_none());
            let status = provider.delivery(1, &mut delivery);
            if unregister {
                assert!(status == DeliveryStatus::Discard);
            } else {
                assert!(status == DeliveryStatus::Current);
                assert_eq!(
                    serde_json::to_value(&delivery.message).unwrap()["error"]["code"],
                    -32002
                );
            }
            provider.delivered(id);
            assert!(!provider.reads.contains_key(&id));
            for ticket in charged {
                provider.admission.complete(ticket.id, Instant::now());
            }
            server.await.unwrap();
            drop(provider);
            drop(view);
            std::fs::remove_dir_all(path).unwrap();
        }
    }
}

async fn lookup_response(provider: &mut DappProvider, method: &str, params: Value) -> Value {
    provider
        .request(
            1,
            "doc".into(),
            "lookup".into(),
            method,
            params,
            Instant::now(),
        )
        .unwrap();
    if !provider.jobs.is_empty() {
        let completion = tokio::time::timeout(Duration::from_secs(5), provider.jobs.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        provider.complete_read(completion);
    }
    result(&messages(provider)).clone()
}

#[tokio::test]
async fn tracked_hashes_stay_local_and_external_hashes_forward_without_waiting() {
    use alloy::primitives::B256;
    use std::sync::Mutex;
    let methods = Arc::new(Mutex::new(Vec::new()));
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        {
            let methods = methods.clone();
            Arc::new(move |request| {
                methods.lock().unwrap().push(request["method"].clone());
                json!({"jsonrpc":"2.0", "id":request["id"], "result":null})
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint);
    let (tracker, context) = crate::public_wallet::test_tracking_context();
    provider.wallet.public_transaction_tracker = tracker;
    let family = context.start_family();
    let hash = B256::repeat_byte(7);
    family.register(hash);
    for method in ["eth_getTransactionByHash", "eth_getTransactionReceipt"] {
        let malformed = lookup_response(&mut provider, method, json!([hash, "extra"])).await;
        assert_eq!(malformed["error"]["code"], -32602);
        let pending = lookup_response(&mut provider, method, json!([hash])).await;
        assert!(pending.get("result").is_some_and(Value::is_null));
    }
    drop(family);
    for method in ["eth_getTransactionByHash", "eth_getTransactionReceipt"] {
        let unavailable = lookup_response(&mut provider, method, json!([hash])).await;
        assert_eq!(unavailable["error"]["code"], -32002);
    }
    assert!(methods.lock().unwrap().is_empty());
    for method in ["eth_getTransactionByHash", "eth_getTransactionReceipt"] {
        let external = lookup_response(&mut provider, method, json!([B256::repeat_byte(8)])).await;
        assert!(external.get("result").is_some_and(Value::is_null));
    }
    assert_eq!(
        *methods.lock().unwrap(),
        vec![
            json!("eth_getTransactionByHash"),
            json!("eth_getTransactionReceipt")
        ]
    );
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn exhausted_block_discovery_unlocks_authorized_hash_lookups() {
    use alloy::{eips::BlockNumHash, primitives::B256};
    use broadcaster_core::query_rpc_pool::QueryRpcPool;
    use std::sync::Mutex;

    let hash = B256::repeat_byte(7);
    let block = BlockNumHash::new(2, B256::repeat_byte(2));
    let (mut canonical, transaction, receipt) = included_lookup_payloads(hash, block);
    canonical["transactions"] = json!([B256::repeat_byte(8), hash]);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        {
            let requests = requests.clone();
            let transaction = transaction.clone();
            let receipt = receipt.clone();
            Arc::new(move |request| {
                let method = request["method"].as_str().unwrap();
                let mut requests = requests.lock().unwrap();
                requests.push((
                    request["method"].clone(),
                    request.get("params").cloned().unwrap_or_else(|| json!([])),
                ));
                let result = match method {
                    "eth_blockNumber" => {
                        if requests.len() == 1 {
                            json!("0x1")
                        } else {
                            json!("0x2")
                        }
                    }
                    "eth_getBlockByNumber" => {
                        assert_eq!(request["params"], json!(["0x2", false]));
                        canonical.clone()
                    }
                    "eth_getBlockReceipts" => {
                        assert_eq!(request["params"], json!([block.hash]));
                        return json!({"jsonrpc":"2.0", "id":request["id"],
                            "error":{"code":-32601,"message":"synthetic unsupported block receipts"}});
                    }
                    "eth_getTransactionByHash" => transaction.clone(),
                    "eth_getTransactionReceipt" => receipt.clone(),
                    _ => panic!("unexpected synthetic RPC method"),
                };
                json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint.clone());
    let http = provider.wallet.http.as_ref().unwrap();
    let pool = Arc::new(QueryRpcPool::with_http_client(
        vec![endpoint],
        Duration::ZERO,
        http.rpc_client.clone(),
    ));
    let observer = crate::block_observer::BlockObserver::establish(pool, 2, http.rpc_broker(), 1)
        .await
        .unwrap();
    let (tracker, context) = crate::public_wallet::test_tracking_context();
    provider.wallet.public_transaction_tracker = tracker.clone();
    let mut guard = context.admit_observer(observer).unwrap();
    guard.register(hash, 0);
    let pending = lookup_response(&mut provider, "eth_getTransactionReceipt", json!([hash])).await;
    assert!(pending.get("result").is_some_and(Value::is_null));
    let mut changes = tracker.subscribe();
    guard.handoff().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while tracker.lookup(1, hash) == crate::PublicTransactionLookup::Pending {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(
        *requests.lock().unwrap(),
        vec![
            (json!("eth_blockNumber"), json!([])),
            (json!("eth_blockNumber"), json!([])),
            (json!("eth_getBlockByNumber"), json!(["0x2", false])),
            (json!("eth_getBlockReceipts"), json!([block.hash])),
        ]
    );
    for (method, expected) in [
        ("eth_getTransactionByHash", transaction),
        ("eth_getTransactionReceipt", receipt),
    ] {
        let response = lookup_response(&mut provider, method, json!([hash])).await;
        assert_eq!(response["result"], expected);
    }
    assert_eq!(
        &requests.lock().unwrap()[4..],
        &[
            (json!("eth_getTransactionByHash"), json!([hash])),
            (json!("eth_getTransactionReceipt"), json!([hash])),
        ]
    );
    tracker.shutdown().await;
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

fn included_lookup_payloads(
    hash: alloy::primitives::B256,
    block: alloy::eips::BlockNumHash,
) -> (Value, Value, Value) {
    use alloy::primitives::{Address, Bloom};
    let canonical: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
    let mut canonical = serde_json::to_value(canonical).unwrap();
    canonical["hash"] = json!(block.hash);
    canonical["number"] = json!(alloy::primitives::U256::from(block.number));
    let transaction = json!({
        "type":"0x2", "hash":hash, "from":Address::ZERO, "to":null,
        "nonce":"0x0", "gas":"0x5208", "value":"0x0", "input":"0x0001",
        "chainId":"0x1", "maxFeePerGas":"0xa", "maxPriorityFeePerGas":"0x1",
        "accessList":[], "v":"0x0", "r":"0x1", "s":"0x1", "yParity":"0x0",
        "blockHash":block.hash, "blockNumber":canonical["number"], "transactionIndex":"0x1",
        "extension":{"owner":"synthetic-lookup-payload", "nested":[null,1]}
    });
    let receipt = json!({
        "type":"0x2", "transactionHash":hash, "transactionIndex":"0x1",
        "blockHash":block.hash, "blockNumber":canonical["number"],
        "from":Address::ZERO, "to":null, "contractAddress":null,
        "gasUsed":"0x5208", "cumulativeGasUsed":"0x5208", "status":"0x1",
        "logs":[], "logsBloom":Bloom::ZERO,
        "extension":{"owner":"synthetic-lookup-payload", "nested":[null,1]}
    });
    (canonical, transaction, receipt)
}

#[tokio::test]
async fn included_lookups_preserve_raw_json_and_never_fallback_when_block_data_disagrees() {
    use alloy::{eips::BlockNumHash, primitives::B256};
    use std::sync::Mutex;
    let hash = B256::repeat_byte(7);
    let block = BlockNumHash::new(12, B256::repeat_byte(12));
    let payloads = included_lookup_payloads(hash, block);
    let scenario = Arc::new(Mutex::new("valid"));
    let methods = Arc::new(Mutex::new(Vec::new()));
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        {
            let scenario = scenario.clone();
            let methods = methods.clone();
            let (canonical, transaction, receipt) = payloads.clone();
            Arc::new(move |request| {
                let method = request["method"].as_str().unwrap();
                methods.lock().unwrap().push(method.to_owned());
                let scenario = *scenario.lock().unwrap();
                let mut result = match method {
                    "eth_getBlockByNumber" => {
                        assert_eq!(request["params"], json!(["0xc", false]));
                        canonical.clone()
                    }
                    "eth_getTransactionByBlockHashAndIndex" => {
                        assert_eq!(request["params"], json!([block.hash, "0x1"]));
                        transaction.clone()
                    }
                    "eth_getBlockReceipts" => {
                        assert_eq!(request["params"], json!([block.hash]));
                        let mut other = receipt.clone();
                        other["transactionHash"] = json!(B256::repeat_byte(9));
                        other["transactionIndex"] = json!("0x0");
                        json!([other, receipt])
                    }
                    _ => panic!("tracked hash must never be forwarded"),
                };
                if method == "eth_getBlockByNumber" {
                    match scenario {
                        "reorg" => result["hash"] = json!(B256::ZERO),
                        "missing_block" => result = Value::Null,
                        _ => {}
                    }
                } else {
                    match scenario {
                        "missing_data" => result = Value::Null,
                        "wrong_location" if method == "eth_getBlockReceipts" => {
                            result[1]["blockHash"] = json!(B256::ZERO);
                        }
                        "wrong_location" => result["transactionIndex"] = json!("0x0"),
                        "duplicate" if method == "eth_getBlockReceipts" => {
                            result = json!([receipt, receipt]);
                        }
                        _ => {}
                    }
                }
                json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint);
    let (tracker, context) = crate::public_wallet::test_tracking_context();
    provider.wallet.public_transaction_tracker = tracker;
    let family = context.start_family();
    family.register(hash);
    family.included(hash, block, 1);
    for method in ["eth_getTransactionByHash", "eth_getTransactionReceipt"] {
        for current in [
            "valid",
            "reorg",
            "missing_block",
            "missing_data",
            "wrong_location",
            "duplicate",
        ] {
            if current == "duplicate" && method == "eth_getTransactionByHash" {
                continue;
            }
            *scenario.lock().unwrap() = current;
            methods.lock().unwrap().clear();
            let response = lookup_response(&mut provider, method, json!([hash])).await;
            if current == "valid" {
                assert_eq!(
                    response["result"],
                    if method == "eth_getTransactionByHash" {
                        payloads.1.clone()
                    } else {
                        payloads.2.clone()
                    }
                );
            } else {
                assert_eq!(response["error"]["code"], -32002);
            }
            let methods = methods.lock().unwrap();
            assert_eq!(methods.len(), 2);
            assert!(
                methods
                    .iter()
                    .any(|method| method == "eth_getBlockByNumber")
            );
        }
    }
    drop(family);
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn included_lookup_drains_after_owner_invalidation_without_delivering_old_payloads() {
    use alloy::{eips::BlockNumHash, primitives::B256};
    let hash = B256::repeat_byte(7);
    let block = BlockNumHash::new(12, B256::repeat_byte(12));
    let (canonical, transaction, _) = included_lookup_payloads(hash, block);
    let (endpoint, started, release, server) = held_responses(
        false,
        BTreeMap::from([
            ("eth_getBlockByNumber", canonical),
            ("eth_getTransactionByBlockHashAndIndex", transaction),
        ]),
    )
    .await;
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint);
    let (tracker, context) = crate::public_wallet::test_tracking_context();
    provider.wallet.public_transaction_tracker = tracker;
    let family = context.start_family();
    family.register(hash);
    family.included(hash, block, 1);
    provider
        .request(
            1,
            "doc".into(),
            "held".into(),
            "eth_getTransactionByHash",
            json!([hash]),
            Instant::now(),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let id = *provider.reads.keys().next().unwrap();
    provider.update_wallet(GatewayWalletState::default(), 3);
    assert_eq!(result(&messages(&mut provider))["error"]["code"], 4100);
    assert!(provider.reads[&id].retired);
    assert!(provider.reads[&id].phase == ReadPhase::Running);
    release.notify_one();
    let completion = tokio::time::timeout(Duration::from_secs(5), provider.jobs.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // Both block-scoped responses completed successfully after the original owner was retired.
    assert_eq!(
        completion.1.as_ref().unwrap()["extension"]["owner"],
        "synthetic-lookup-payload"
    );
    provider.complete_read(completion);
    assert!(messages(&mut provider).is_empty());
    assert!(!provider.reads.contains_key(&id));
    server.await.unwrap();
    drop(family);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

fn seed_balance(
    provider: &mut DappProvider,
    asset: crate::PublicBalanceAsset,
    observed_at: Instant,
) -> (crate::PublicBalanceScope, PublicAccountMetadata) {
    let permission = &provider.permissions[0];
    let (_, account) = provider.resolve(&permission.origin).unwrap();
    provider.wallet.public_accounts = vec![account.clone()];
    // Publish the fixture's tracked accounts before admitting reads against them.
    provider
        .authority_fallback
        .as_ref()
        .unwrap()
        .send_replace(provider.wallet.clone());
    let scope = crate::PublicBalanceScope::new(
        provider
            .wallet
            .view
            .as_ref()
            .unwrap()
            .wallet_id()
            .to_owned(),
        provider.wallet.active_wallet_generation,
        provider.wallet.http.as_ref().unwrap().rpc_broker(),
        provider.wallet.routes[&permission.chain_id].clone(),
    );
    let cache = &provider.wallet.public_balance_cache;
    cache.set_scope(scope.clone());
    let ticket = cache.begin_refresh(&scope, account.status).unwrap();
    assert!(
        cache
            .finish_refresh(
                ticket,
                Some(crate::PublicBalanceSnapshot {
                    chain_id: permission.chain_id,
                    refreshed_at: std::time::SystemTime::now(),
                    accounts: vec![crate::PublicAccountBalance {
                        account: account.clone(),
                        balances: vec![crate::PublicBalanceEntry {
                            asset,
                            amount: crate::PublicBalanceAmount::Available(
                                alloy::primitives::U256::from(7)
                            ),
                        }],
                        observed_at: Some(observed_at.into_std()),
                        observed_block: Some(alloy::eips::BlockNumHash::new(
                            10,
                            alloy::primitives::B256::repeat_byte(10)
                        )),
                    }],
                })
            )
            .accepted
    );
    (scope, account)
}

async fn balance_rpc() -> (
    url::Url,
    Arc<std::sync::Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    use alloy::sol_types::SolValue;
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        {
            let requests = requests.clone();
            Arc::new(move |request| {
                requests.lock().unwrap().push(request.clone());
                let result = match request["method"].as_str().unwrap() {
                    "eth_getBalance" => json!("0x63"),
                    "eth_call" => json!(alloy::primitives::Bytes::from(
                        alloy::primitives::U256::from(99).abi_encode()
                    )),
                    _ => panic!("unexpected balance RPC"),
                };
                json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    (endpoint, requests, server)
}

#[tokio::test]
async fn native_local_balance_requires_current_authorized_observation() {
    let (endpoint, requests, server) = balance_rpc().await;
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint);
    let native = crate::public_wallet::native_asset_for_chain(1).unwrap();
    let (_, account) = seed_balance(&mut provider, native.clone(), Instant::now());
    let params = json!([account.address, "latest"]);
    assert!(provider.wallet.token_registry.is_none());
    assert_eq!(
        lookup_response(&mut provider, "eth_getBalance", params.clone()).await["result"],
        "0x7"
    );
    assert!(requests.lock().unwrap().is_empty());
    provider
        .register(
            2,
            PeerId::from_bytes([8; 16]),
            "unapproved".into(),
            "https://unapproved.invalid/",
        )
        .unwrap();
    messages(&mut provider);
    provider
        .request(
            2,
            "unapproved".into(),
            "denied".into(),
            "eth_getBalance",
            params.clone(),
            Instant::now(),
        )
        .unwrap();
    assert_eq!(result(&messages(&mut provider))["error"]["code"], 4100);
    assert!(requests.lock().unwrap().is_empty());
    // Native ingress still requires its selector, even when a fresh local value exists.
    assert_eq!(
        lookup_response(&mut provider, "eth_getBalance", json!([account.address])).await["error"]["code"],
        -32602
    );
    for case in [
        "stale",
        "pending",
        "scope",
        "historical",
        "other-account",
        "untracked",
    ] {
        provider.wallet.public_balance_cache.clear();
        let now = Instant::now();
        let observed = if case == "stale" {
            now - Duration::from_secs(31)
        } else {
            now
        };
        let (scope, account) = seed_balance(&mut provider, native.clone(), observed);
        let mut params = params.clone();
        match case {
            "pending" => provider
                .wallet
                .public_balance_cache
                .invalidate_account(&scope, &account, true, None),
            "scope" => {
                provider
                    .wallet
                    .public_balance_cache
                    .set_scope(crate::PublicBalanceScope::new(
                        view.wallet_id().to_owned(),
                        provider.wallet.active_wallet_generation + 1,
                        provider.wallet.http.as_ref().unwrap().rpc_broker(),
                        provider.wallet.routes[&1].clone(),
                    ));
            }
            "historical" => params[1] = json!("pending"),
            "other-account" => params[0] = json!(alloy::primitives::Address::ZERO),
            "untracked" => {
                provider.wallet.public_accounts.clear();
                provider
                    .authority_fallback
                    .as_ref()
                    .unwrap()
                    .send_replace(provider.wallet.clone());
            }
            _ => {}
        }
        let before = requests.lock().unwrap().len();
        assert_eq!(
            lookup_response(&mut provider, "eth_getBalance", params.clone()).await["result"],
            "0x63",
            "{case}"
        );
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), before + 1, "{case}");
        assert_eq!(recorded.last().unwrap()["params"], params, "{case}");
    }
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn local_balance_delivery_rechecks_generation_and_namespace() {
    let (path, mut provider, view) = fixture();
    authorize(
        &mut provider,
        &view,
        url::Url::parse("http://127.0.0.1:1").unwrap(),
    );
    let native = crate::public_wallet::native_asset_for_chain(1).unwrap();
    for case in ["pending", "refreshed-generation", "replaced-namespace"] {
        provider.wallet.public_balance_cache.clear();
        let (scope, account) = seed_balance(&mut provider, native.clone(), Instant::now());
        provider
            .request(
                1,
                "doc".into(),
                "local".into(),
                "eth_getBalance",
                json!([account.address, "latest"]),
                Instant::now(),
            )
            .unwrap();
        assert!(provider.jobs.is_empty());
        let (_, mut delivery) = provider
            .drain()
            .into_iter()
            .find(|(_, delivery)| delivery.ticket_id().is_some())
            .unwrap();
        assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Current);
        if case == "replaced-namespace" {
            provider.wallet.public_balance_cache.clear();
        } else {
            provider.wallet.public_balance_cache.invalidate_account(
                &scope,
                &account,
                case == "pending",
                None,
            );
        }
        if case != "pending" {
            seed_balance(&mut provider, native.clone(), Instant::now());
        }
        assert!(
            provider.delivery(1, &mut delivery) == DeliveryStatus::Changed,
            "{case}"
        );
        assert!(delivery.local_error);
        assert_eq!(
            serde_json::to_value(&delivery.message).unwrap()["error"]["code"],
            -32002
        );
        assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Current);
        provider.delivered(delivery.ticket_id().unwrap());
    }
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn queued_balance_loses_eligibility_before_dispatch_and_forwards() {
    let (endpoint, requests, server) = balance_rpc().await;
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint);
    let (scope, account) = seed_balance(
        &mut provider,
        crate::public_wallet::native_asset_for_chain(1).unwrap(),
        Instant::now(),
    );
    for index in 0..32 {
        provider
            .request(
                1,
                "doc".into(),
                format!("held-{index}"),
                "eth_accounts",
                json!([]),
                Instant::now(),
            )
            .unwrap();
    }
    provider
        .request(
            1,
            "doc".into(),
            "balance".into(),
            "eth_getBalance",
            json!([account.address, "latest"]),
            Instant::now(),
        )
        .unwrap();
    assert!(
        provider
            .reads
            .values()
            .any(|read| read.owner.request_id == "balance" && read.phase == ReadPhase::Queued)
    );
    provider
        .wallet
        .public_balance_cache
        .invalidate_account(&scope, &account, true, None);
    messages(&mut provider);
    let completion = tokio::time::timeout(Duration::from_secs(5), provider.jobs.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    provider.complete_read(completion);
    assert_eq!(result(&messages(&mut provider))["result"], "0x63");
    assert_eq!(requests.lock().unwrap().len(), 1);
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn token_local_balance_preserves_ingress_and_forwards_ineligible_calls() {
    use alloy::primitives::{Address, Bytes, U256};
    use alloy::sol_types::SolValue;
    let (endpoint, requests, server) = balance_rpc().await;
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint);
    let token = Address::repeat_byte(0x42);
    let asset = crate::PublicBalanceAsset {
        id: crate::PublicAssetId::Erc20(token),
        symbol: "TEST".into(),
        decimals: 6,
    };
    let (_, account) = seed_balance(&mut provider, asset, Instant::now());
    provider.wallet.token_registry = Some(Arc::new(crate::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([(
            (1, token.to_string().to_ascii_lowercase()),
            crate::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: token.to_string(),
                symbol: "TEST".into(),
                decimals: 6,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]),
    }));
    // Standard selector with an Alloy-encoded address word.
    let calldata = Bytes::from(
        [
            alloy::primitives::hex!("70a08231").as_slice(),
            account.address.abi_encode().as_slice(),
        ]
        .concat(),
    );
    let call = json!({"to":token, "data":calldata});
    for params in [
        json!([call]),
        json!([call, "latest"]),
        json!([{"to":token, "input":calldata, "data":calldata}, "latest"]),
    ] {
        assert_eq!(
            lookup_response(&mut provider, "eth_call", params).await["result"],
            json!(Bytes::from(U256::from(7).abi_encode()))
        );
    }
    assert!(requests.lock().unwrap().is_empty());
    for (params, code) in [
        (
            json!([{"to":token, "input":calldata, "data":"0x01"}]),
            -32602,
        ),
        (
            json!([{"to":token, "data":calldata, "chainId":"0x2"}]),
            -32000,
        ),
        (
            json!([{"to":token, "data":calldata, "gas":"invalid"}]),
            -32602,
        ),
        (json!([call, "latest", null]), -32602),
    ] {
        assert_eq!(
            lookup_response(&mut provider, "eth_call", params).await["error"]["code"],
            code
        );
        assert!(provider.jobs.is_empty());
        assert!(requests.lock().unwrap().is_empty());
    }
    let mut cases = Vec::new();
    for (field, value) in [
        ("from", json!(Address::ZERO)),
        ("value", json!("0x0")),
        ("gas", Value::Null),
        ("accessList", json!([])),
        ("context", Value::Null),
    ] {
        let mut context = call.clone();
        context[field] = value;
        cases.push(json!([context, "latest"]));
    }
    let trailing = Bytes::from([calldata.as_ref(), &[0]].concat());
    let mut noncanonical = calldata.to_vec();
    noncanonical[4] = 1;
    let allowance = Bytes::from(
        [
            alloy::primitives::hex!("dd62ed3e").as_slice(),
            account.address.abi_encode().as_slice(),
            token.abi_encode().as_slice(),
        ]
        .concat(),
    );
    cases.extend([
        json!([call, "latest", {}]),
        json!([call, "pending"]),
        json!([call, {"blockNumber":"0xa"}]),
        json!([{"to":Address::repeat_byte(0x43), "data":calldata}, "latest"]),
        json!([{"to":token, "data":trailing}, "latest"]),
        json!([{"to":token, "data":Bytes::from(noncanonical)}, "latest"]),
        json!([{"to":token, "data":allowance}, "latest"]),
        json!([{"to":null, "data":calldata}, "latest"]),
    ]);
    for params in cases {
        let expected = RpcRead::from_method_params("eth_call", params.clone(), 1).unwrap();
        let before = requests.lock().unwrap().len();
        assert_eq!(
            lookup_response(&mut provider, "eth_call", params).await["result"],
            json!(Bytes::from(U256::from(99).abi_encode()))
        );
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), before + 1);
        let forwarded = recorded.last().unwrap();
        assert_eq!(forwarded["method"], "eth_call");
        assert_eq!(
            RpcRead::from_method_params("eth_call", forwarded["params"].clone(), 1).unwrap(),
            expected
        );
    }
    // Registry publication must invalidate a prepared token answer without revoking unrelated reads.
    for remove in [false, true] {
        let mut wallet = provider.wallet.clone();
        Arc::make_mut(wallet.token_registry.as_mut().unwrap())
            .tokens
            .values_mut()
            .next()
            .unwrap()
            .decimals = 6;
        provider.update_wallet(wallet, provider.generation);
        messages(&mut provider);
        provider
            .request(
                1,
                "doc".into(),
                "registry".into(),
                "eth_call",
                json!([call]),
                Instant::now(),
            )
            .unwrap();
        let (_, mut delivery) = provider
            .drain()
            .into_iter()
            .find(|(_, delivery)| delivery.ticket_id().is_some())
            .unwrap();
        assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Current);
        let mut changed = provider.wallet.clone();
        if remove {
            Arc::make_mut(changed.token_registry.as_mut().unwrap())
                .tokens
                .clear();
        } else {
            Arc::make_mut(changed.token_registry.as_mut().unwrap())
                .tokens
                .values_mut()
                .next()
                .unwrap()
                .decimals = 18;
        }
        assert!(provider.wallet.same_authority(&changed));
        assert!(!provider.wallet.same_state(&changed));
        provider.update_wallet(changed, provider.generation);
        assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Changed);
        assert!(delivery.local_error);
        assert_eq!(
            serde_json::to_value(&delivery.message).unwrap()["error"]["code"],
            -32002
        );
        provider.delivered(delivery.ticket_id().unwrap());
        messages(&mut provider);
        assert_eq!(
            lookup_response(&mut provider, "eth_call", json!([call])).await["result"],
            json!(Bytes::from(U256::from(99).abi_encode()))
        );
    }
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

fn personal_approval(provider: &mut DappProvider, request_id: &str, now: Instant) {
    let account = provider
        .resolve(&provider.documents[&(1, "doc".to_owned())].origin)
        .unwrap()
        .1
        .address;
    provider
        .request(
            1,
            "doc".to_owned(),
            request_id.to_owned(),
            "personal_sign",
            json!(["0x1234", account]),
            now,
        )
        .unwrap();
}

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
async fn native_approval_decisions_and_delivery_remain_bound_to_original_authority() {
    for invalidation in ["document", "revoke", "wallet", "network", "immediate"] {
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
                provider.revoke(&ready.permission.permission_id).unwrap();
            }
            "wallet" => {
                let mut wallet = provider.wallet.clone();
                wallet.active_wallet_generation += 1;
                provider.update_wallet(wallet, 3);
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
        reply,
    );
    assert_eq!(rejected.await.unwrap(), Err(RpcBrokerError::OriginRejected));
    let (reply, response) = oneshot::channel();
    provider.approval_read(
        &ready.id,
        endpoint.clone().into(),
        rpc(),
        Instant::now(),
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

fn approval_handle(
    provider: &DappProvider,
) -> (
    crate::gateway::GatewayHandle,
    tokio::sync::mpsc::Receiver<crate::gateway::Command>,
) {
    let (commands, receiver) = tokio::sync::mpsc::channel(32);
    let (_, snapshots) = tokio::sync::watch::channel(crate::gateway::GatewaySnapshot {
        config: crate::gateway::GatewayConfig::default(),
        listener_addr: None,
        peers: Vec::new(),
        pairing_active: false,
        locked: false,
        generation: 0,
        error: None,
        permissions: Vec::new(),
    });
    (
        crate::gateway::GatewayHandle {
            commands,
            ui_events: tokio::sync::broadcast::channel(32).0,
            snapshots,
            approvals: provider.approval_updates.subscribe(),
            wallet_switches: provider.switch_updates.subscribe(),
            authority: provider.authority_fallback.as_ref().unwrap().clone(),
        },
        receiver,
    )
}

#[tokio::test]
async fn native_review_fee_fanout_succeeds_with_full_dapp_queue() {
    use crate::gateway::Command;
    let (path, mut provider, view) = fixture();
    let responses = BTreeMap::from([
        (
            "eth_feeHistory",
            json!({"oldestBlock": "0x1", "baseFeePerGas": ["0x1", "0x2"], "gasUsedRatio": [0.5], "reward": [["0x1"]]}),
        ),
        ("eth_gasPrice", json!("0x6")),
        ("eth_maxPriorityFeePerGas", json!("0x7")),
    ]);
    let mut endpoints = Vec::new();
    let mut servers = Vec::new();
    for _ in 0..23 {
        let (endpoint, _, release, server) = held_responses(false, responses.clone()).await;
        release.notify_one();
        endpoints.push(endpoint);
        servers.push(server);
    }
    authorize(&mut provider, &view, endpoints[0].clone());
    let mut wallet = provider.wallet.clone();
    wallet
        .routes
        .insert(1, RpcChainRoute::new(1, endpoints.clone()));
    provider.update_wallet(wallet, 3);
    personal_approval(&mut provider, "sign", Instant::now());
    let ready = provider.approval_updates.borrow()[0].clone();
    let now = Instant::now();
    // Fill active slots and the entire queue, allowing the rate bucket to refill.
    for _ in 0..32 {
        assert!(matches!(
            provider
                .admission
                .admit(ready.origin.clone(), now - Duration::from_secs(2)),
            Ok(ReadAdmissionDecision::Ready(_))
        ));
    }
    for entered in [now - Duration::from_secs(2), now - Duration::from_secs(1)] {
        for _ in 0..32 {
            assert!(matches!(
                provider.admission.admit(ready.origin.clone(), entered),
                Ok(ReadAdmissionDecision::Queued(_))
            ));
        }
    }
    assert_eq!(
        result(&request(
            &mut provider,
            1,
            "doc",
            "excess",
            "eth_blockNumber",
            now
        ))["error"]["code"],
        -32005
    );
    let (handle, mut commands) = approval_handle(&provider);
    let mut callers = tokio::task::JoinSet::new();
    for endpoint in endpoints {
        for (&method, expected) in &responses {
            let reads = handle.approval_reads(ready.id.clone());
            let endpoint = endpoint.clone();
            let expected = expected.clone();
            let params = if method == "eth_feeHistory" {
                json!(["0x1", "latest", [50.0]])
            } else {
                json!([])
            };
            callers.spawn(async move {
                let result = reads
                    .read(
                        endpoint.into(),
                        RpcRead::from_method_params(method, params, 1).unwrap(),
                    )
                    .await;
                (result, expected)
            });
        }
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while !callers.is_empty() {
            tokio::select! {
                command = commands.recv() => {
                    let Command::ApprovalRead(id, endpoint, rpc, entered, reply) = command.unwrap() else {
                        panic!("expected approval read");
                    };
                    provider.approval_read(&id, endpoint, rpc, entered, reply);
                }
                completion = provider.jobs.join_next(), if !provider.jobs.is_empty() => {
                    provider.complete_read(completion.unwrap().unwrap());
                }
                result = callers.join_next() => {
                    let (actual, expected) = result.unwrap().unwrap();
                    assert_eq!(actual, Ok(expected), "wallet fee reads must bypass the full dapp queue");
                }
            }
        }
        for server in servers {
            server.await.unwrap();
        }
    }).await.unwrap();
    assert!(provider.reads.is_empty());
    assert_eq!(
        result(&request(
            &mut provider,
            1,
            "doc",
            "still-excess",
            "eth_blockNumber",
            Instant::now()
        ))["error"]["code"],
        -32005
    );
    drop(handle);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn native_review_read_waits_for_command_capacity_and_delivers_once() {
    use crate::gateway::Command;
    let (path, mut provider, view) = fixture();
    let endpoint = url::Url::parse("http://127.0.0.1:1").unwrap();
    authorize(&mut provider, &view, endpoint.clone());
    personal_approval(&mut provider, "sign", Instant::now());
    let ready = provider.approval_updates.borrow()[0].clone();
    let (handle, mut commands) = approval_handle(&provider);
    for _ in 0..32 {
        handle
            .commands
            .try_send(Command::Pair(oneshot::channel().0))
            .ok()
            .unwrap();
    }
    let reads = handle.approval_reads(ready.id.clone());
    let mut read = Box::pin(reads.read(
        endpoint.into(),
        RpcRead::from_method_params("eth_blockNumber", json!([]), 1).unwrap(),
    ));
    assert!(futures_util::poll!(&mut read).is_pending());
    assert!(matches!(commands.try_recv().unwrap(), Command::Pair(_)));
    assert!(futures_util::poll!(&mut read).is_pending());
    for _ in 0..31 {
        assert!(matches!(commands.try_recv().unwrap(), Command::Pair(_)));
    }
    let Command::ApprovalRead(_, _, _, _, reply) = commands.try_recv().unwrap() else {
        panic!("expected the waiting approval read");
    };
    reply.send(Ok(json!("0xfeed"))).unwrap();
    assert_eq!(read.await, Ok(json!("0xfeed")));
    assert!(commands.try_recv().is_err(), "read must only enqueue once");
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test(start_paused = true)]
async fn native_review_read_cancellation_expiry_and_drop_prevent_late_enqueue() {
    use crate::gateway::Command;
    for stop in ["cancelled", "expired", "dropped"] {
        let (path, mut provider, view) = fixture();
        let endpoint = url::Url::parse("http://127.0.0.1:1").unwrap();
        authorize(&mut provider, &view, endpoint.clone());
        personal_approval(&mut provider, "sign", Instant::now());
        let ready = provider.approval_updates.borrow()[0].clone();
        if stop == "expired" {
            tokio::time::advance(
                (ready.deadline - Instant::now())
                    .checked_sub(Duration::from_secs(1))
                    .unwrap(),
            )
            .await;
        }
        let (handle, mut commands) = approval_handle(&provider);
        for _ in 0..32 {
            handle
                .commands
                .try_send(Command::Pair(oneshot::channel().0))
                .ok()
                .unwrap();
        }
        let reads = handle.approval_reads(ready.id.clone());
        let mut read = Box::pin(reads.read(
            endpoint.into(),
            RpcRead::from_method_params("eth_blockNumber", json!([]), 1).unwrap(),
        ));
        assert!(futures_util::poll!(&mut read).is_pending());
        let read = match stop {
            "cancelled" => {
                ready.control.invalidate(&RpcBrokerError::OriginRejected);
                Some((read, RpcBrokerError::OriginRejected))
            }
            "expired" => {
                tokio::time::advance(Duration::from_secs(1)).await;
                Some((read, RpcBrokerError::Timeout))
            }
            "dropped" => {
                drop(read);
                None
            }
            _ => unreachable!(),
        };
        // Capacity becomes available before the cancelled waiter is polled again.
        for _ in 0..32 {
            assert!(matches!(commands.try_recv().unwrap(), Command::Pair(_)));
        }
        if let Some((read, expected)) = read {
            assert_eq!(read.await, Err(expected));
        }
        tokio::task::yield_now().await;
        assert!(
            commands.try_recv().is_err(),
            "stopped reads must not enqueue"
        );
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn native_review_read_queue_and_response_wait_share_original_deadline() {
    use crate::gateway::{Command, admission::READ_TIMEOUT};
    for waiting_for_response in [false, true] {
        let (path, mut provider, view) = fixture();
        let endpoint = url::Url::parse("http://127.0.0.1:1").unwrap();
        authorize(&mut provider, &view, endpoint.clone());
        personal_approval(&mut provider, "sign", Instant::now());
        let ready = provider.approval_updates.borrow()[0].clone();
        let (handle, mut commands) = approval_handle(&provider);
        for _ in 0..32 {
            handle
                .commands
                .try_send(Command::Pair(oneshot::channel().0))
                .ok()
                .unwrap();
        }
        let reads = handle.approval_reads(ready.id.clone());
        let mut read = Box::pin(reads.read(
            endpoint.into(),
            RpcRead::from_method_params("eth_blockNumber", json!([]), 1).unwrap(),
        ));
        let entered = Instant::now();
        assert!(futures_util::poll!(&mut read).is_pending());
        tokio::time::advance(READ_TIMEOUT.checked_sub(Duration::from_secs(1)).unwrap()).await;
        let reply = if waiting_for_response {
            for _ in 0..32 {
                assert!(matches!(commands.try_recv().unwrap(), Command::Pair(_)));
            }
            assert!(futures_util::poll!(&mut read).is_pending());
            let Command::ApprovalRead(_, _, _, command_entered, reply) =
                commands.try_recv().unwrap()
            else {
                panic!("expected the waiting approval read");
            };
            assert_eq!(command_entered, entered, "actor must retain the queue wait");
            Some(reply)
        } else {
            None
        };
        tokio::time::advance(Duration::from_secs(1)).await;
        if !waiting_for_response {
            for _ in 0..32 {
                assert!(matches!(commands.try_recv().unwrap(), Command::Pair(_)));
            }
        }
        assert_eq!(
            read.await,
            Err(if waiting_for_response {
                RpcBrokerError::Timeout
            } else {
                RpcBrokerError::TimeoutBeforeDispatch
            })
        );
        assert_eq!(Instant::now(), entered + READ_TIMEOUT);
        if let Some(reply) = reply {
            assert!(reply.is_closed());
        }
        assert!(
            commands.try_recv().is_err(),
            "expired reads must not enqueue"
        );
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn native_completed_decisions_wait_for_command_capacity_without_reexecution() {
    use crate::gateway::{Command, GatewayApprovalFailure};
    for decision in ["reject", "success", "fallback"] {
        let (path, mut provider, view) = fixture();
        authorize(
            &mut provider,
            &view,
            url::Url::parse("http://127.0.0.1:1").unwrap(),
        );
        personal_approval(&mut provider, "sign", Instant::now());
        let ready = provider.approval_updates.borrow()[0].clone();
        if decision != "reject" {
            provider.begin_approval(&ready.id).unwrap();
        }
        let (handle, mut commands) = approval_handle(&provider);
        for _ in 0..32 {
            handle
                .commands
                .try_send(Command::Pair(tokio::sync::oneshot::channel().0))
                .ok()
                .unwrap();
        }
        let mut completion: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), LocalProviderFailure>>>,
        > = if decision == "fallback" {
            Box::pin(handle.return_approval_to_review(ready.id.clone()))
        } else {
            Box::pin(handle.complete_approval(
                ready.id.clone(),
                if decision == "reject" {
                    Err(GatewayApprovalFailure::Local(
                        LocalProviderFailure::UserRejected,
                    ))
                } else {
                    Ok(json!("already-signed"))
                },
            ))
        };
        assert!(futures_util::poll!(&mut completion).is_pending());
        for _ in 0..32 {
            assert!(matches!(commands.try_recv().unwrap(), Command::Pair(_)));
        }
        assert!(futures_util::poll!(&mut completion).is_pending());
        match commands.try_recv().unwrap() {
            Command::CompleteApproval(id, outcome, reply) => {
                reply
                    .send(provider.complete_approval(&id, outcome))
                    .unwrap();
            }
            Command::ReturnApprovalToReview(id, reply) => {
                reply.send(provider.return_approval_to_review(&id)).unwrap();
            }
            _ => panic!("unexpected command"),
        }
        assert_eq!(completion.await, Ok(()));
        assert!(
            commands.try_recv().is_err(),
            "decision must only enqueue once"
        );
        if decision == "fallback" {
            assert_eq!(provider.approvals.len(), 1);
            assert!(provider.begin_approval(&ready.id).is_ok());
            assert_eq!(
                provider.approval_updates.borrow()[0].deadline,
                ready.deadline
            );
            assert!(
                messages(&mut provider)
                    .iter()
                    .all(|(_, value)| value["type"] != "provider_response")
            );
        } else {
            let delivered = messages(&mut provider);
            let outcome = result(&delivered);
            if decision == "reject" {
                assert_eq!(outcome["error"]["code"], 4001);
            } else {
                assert_eq!(outcome["result"], "already-signed");
            }
            assert!(provider.approvals.is_empty());
        }
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn native_decision_waits_are_bounded_by_original_control() {
    use crate::gateway::Command;
    for waiting_for_ack in [false, true] {
        for expire in [false, true] {
            let (path, mut provider, view) = fixture();
            authorize(
                &mut provider,
                &view,
                url::Url::parse("http://127.0.0.1:1").unwrap(),
            );
            personal_approval(&mut provider, "sign", Instant::now());
            let ready = provider.approval_updates.borrow()[0].clone();
            provider.begin_approval(&ready.id).unwrap();
            let (handle, mut commands) = approval_handle(&provider);
            if !waiting_for_ack {
                for _ in 0..32 {
                    handle
                        .commands
                        .try_send(Command::Pair(tokio::sync::oneshot::channel().0))
                        .ok()
                        .unwrap();
                }
            }
            let mut completion =
                Box::pin(handle.complete_approval(ready.id.clone(), Ok(json!("signed-once"))));
            assert!(futures_util::poll!(&mut completion).is_pending());
            let held = if waiting_for_ack {
                Some(commands.try_recv().unwrap())
            } else {
                None
            };
            if expire {
                tokio::time::advance(ready.deadline - Instant::now()).await;
            } else {
                ready.control.invalidate(&RpcBrokerError::OriginRejected);
            }
            assert_eq!(completion.await, Err(LocalProviderFailure::Unavailable));
            if let Some(Command::CompleteApproval(id, outcome, reply)) = held {
                assert!(reply.is_closed());
                assert!(provider.complete_approval(&id, outcome).is_err());
            }
            drop(provider);
            drop(view);
            std::fs::remove_dir_all(path).unwrap();
        }
    }
}

#[tokio::test]
async fn native_broker_completion_preserves_payload_and_revalidates_delivery_owner() {
    use crate::gateway::GatewayApprovalFailure;
    let payload = json!({"code": 4001, "message": "remote nonce rejection",
        "data": {"nested": [null, false, {"value": 3}]}, "extension": ["retained"]});
    let remote = crate::RpcRemoteError::from(
        serde_json::from_value::<
            alloy::serde::WithOtherFields<alloy::rpc::json_rpc::ErrorPayload<serde_json::Value>>,
        >(payload.clone())
        .unwrap(),
    );
    for broker in [
        RpcBrokerError::Remote(remote.clone()),
        RpcBrokerError::InnerRevert(crate::RpcRevert::from_individual(
            alloy::primitives::Bytes::from_static(&[1, 2]),
            remote,
        )),
    ] {
        for invalidate in [false, true] {
            let (path, mut provider, view) = fixture();
            authorize(
                &mut provider,
                &view,
                url::Url::parse("http://127.0.0.1:1").unwrap(),
            );
            personal_approval(&mut provider, "sign", Instant::now());
            let ready = provider.approval_updates.borrow()[0].clone();
            let failure = GatewayApprovalFailure::Broker(broker.clone());
            assert_eq!(
                provider.complete_approval(&ready.id, Err(failure.clone())),
                Err(LocalProviderFailure::Unavailable)
            );
            provider.begin_approval(&ready.id).unwrap();
            let (handle, mut commands) = approval_handle(&provider);
            let mut completion = Box::pin(handle.complete_approval(ready.id.clone(), Err(failure)));
            assert!(futures_util::poll!(&mut completion).is_pending());
            let crate::gateway::Command::CompleteApproval(id, failure, reply) =
                commands.try_recv().unwrap()
            else {
                panic!("expected typed native completion");
            };
            reply
                .send(provider.complete_approval(&id, failure))
                .unwrap();
            assert_eq!(completion.await, Ok(()));
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
            if invalidate {
                provider.unregister(1, "doc");
            }
            let status = provider.delivery(1, &mut delivery);
            if invalidate {
                assert!(status != DeliveryStatus::Current);
            } else {
                assert!(status == DeliveryStatus::Current);
                assert_eq!(
                    serde_json::to_value(delivery.message).unwrap()["error"],
                    payload
                );
            }
            drop(provider);
            drop(view);
            std::fs::remove_dir_all(path).unwrap();
        }
    }
}

#[tokio::test]
async fn native_chain_policy_commits_only_current_confirmation_and_retires_old_scope() {
    for decision in ["confirm", "reject", "stale", "unsupported"] {
        let (path, mut provider, view) = fixture();
        let (_, original) = authorize(
            &mut provider,
            &view,
            url::Url::parse("http://127.0.0.1:1").unwrap(),
        );
        let mut wallet = provider.wallet.clone();
        wallet.routes.insert(
            10,
            RpcChainRoute::new(10, vec![url::Url::parse("http://127.0.0.1:2").unwrap()]),
        );
        provider.update_wallet(wallet, 3);
        messages(&mut provider);
        personal_approval(&mut provider, "old-sign", Instant::now());
        let old = provider.approval_updates.borrow()[0].clone();
        for index in 0..32 {
            provider
                .request(
                    1,
                    "doc".into(),
                    format!("held-{index}"),
                    "eth_accounts",
                    json!([]),
                    Instant::now(),
                )
                .unwrap();
        }
        provider
            .request(
                1,
                "doc".into(),
                "queued-read".into(),
                "eth_blockNumber",
                json!([]),
                Instant::now(),
            )
            .unwrap();
        assert!(
            provider
                .reads
                .values()
                .any(|read| read.owner.request_id == "queued-read"
                    && read.phase == ReadPhase::Queued)
        );
        let chain = if decision == "unsupported" {
            "0x89"
        } else {
            "0xa"
        };
        provider
            .request(
                1,
                "doc".into(),
                "switch".into(),
                "wallet_switchEthereumChain",
                json!([{ "chainId": chain }]),
                Instant::now(),
            )
            .unwrap();
        if decision == "unsupported" {
            let output = messages(&mut provider);
            assert_eq!(
                output
                    .iter()
                    .find(|(_, message)| message["request_id"] == "switch")
                    .unwrap()
                    .1["error"]["code"],
                4901
            );
        } else {
            let ready = provider
                .approval_updates
                .borrow()
                .iter()
                .find(|request| request.id != old.id)
                .unwrap()
                .clone();
            provider.begin_approval(&ready.id).unwrap();
            if decision == "stale" {
                provider
                    .authority_fallback
                    .as_ref()
                    .unwrap()
                    .send_replace(GatewayWalletState::default());
                assert!(
                    provider
                        .complete_approval(&ready.id, Ok(Value::Null))
                        .is_err()
                );
            } else {
                let outcome = if decision == "reject" {
                    Err(crate::gateway::GatewayApprovalFailure::Local(
                        LocalProviderFailure::UserRejected,
                    ))
                } else {
                    Ok(Value::Null)
                };
                provider.complete_approval(&ready.id, outcome).unwrap();
                let output = messages(&mut provider);
                let response = output
                    .iter()
                    .position(|(_, message)| message["request_id"] == "switch")
                    .unwrap();
                if decision == "confirm" {
                    let changed = output
                        .iter()
                        .position(|(_, message)| {
                            message["type"] == "provider_state" && message["chain_id"] == "0xa"
                        })
                        .unwrap();
                    assert!(changed < response);
                    assert!(output[response].1["result"].is_null());
                    assert!(output[response].1.get("error").is_none());
                    assert!(old.control.ensure_current().is_err());
                    assert!(
                        !provider
                            .reads
                            .values()
                            .any(|read| read.owner.request_id == "queued-read")
                    );
                    assert!(provider.jobs.is_empty());
                } else {
                    assert_eq!(output[response].1["error"]["code"], 4001);
                }
            }
        }
        let stored = provider
            .store
            .list_gateway_permissions(&view)
            .unwrap()
            .remove(0);
        assert_eq!(stored.chain_id, if decision == "confirm" { 10 } else { 1 });
        assert_eq!(stored.public_account_uuid, original.public_account_uuid);
        assert_eq!(stored.public_account_scope, original.public_account_scope);
        assert_eq!(
            stored.owning_private_wallet_uuid,
            original.owning_private_wallet_uuid
        );
        assert_eq!(provider.wallet.default_chain_id, Some(1));
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn add_chain_acknowledges_saved_configuration_without_mutating_authority() {
    let (path, mut provider, view) = fixture();
    authorize(
        &mut provider,
        &view,
        url::Url::parse("http://127.0.0.1:1").unwrap(),
    );
    let before = provider.wallet.clone();
    let permissions = provider.permissions.clone();
    provider.request(1, "doc".into(), "add".into(), "wallet_addEthereumChain", json!([{ "chainId": "0x1", "rpcUrls": ["https://ignored.invalid"], "custom": { "retained": true } }]), Instant::now()).unwrap();
    assert!(
        messages(&mut provider)
            .iter()
            .all(|(_, message)| message["type"] != "provider_response")
    );
    let ready = provider.approval_updates.borrow()[0].clone();
    provider.begin_approval(&ready.id).unwrap();
    provider
        .complete_approval(&ready.id, Ok(Value::Null))
        .unwrap();
    let output = messages(&mut provider);
    assert!(result(&output)["result"].is_null());
    assert!(result(&output).get("error").is_none());
    assert!(before.same_state(&provider.wallet));
    assert!(provider.permissions == permissions);
    provider
        .request(
            1,
            "doc".into(),
            "disabled".into(),
            "wallet_addEthereumChain",
            json!([{ "chainId": "0xa" }]),
            Instant::now(),
        )
        .unwrap();
    assert_eq!(result(&messages(&mut provider))["error"]["code"], 4901);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn watch_asset_recognition_keeps_shared_slot_and_never_responds_twice() {
    let (path, mut provider, view) = fixture();
    authorize(
        &mut provider,
        &view,
        url::Url::parse("http://127.0.0.1:1").unwrap(),
    );
    let params = json!({ "type": "ERC20", "options": { "address": "0x0000000000000000000000000000000000000001", "symbol": "UNTRUSTED", "image": "https://ignored.invalid" } });
    for (id, params, code) in [
        ("type", json!({ "type": "ERC721", "options": {} }), 4200),
        (
            "chain",
            json!({ "type": "ERC20", "chainId": "0xa", "options": { "address": "0x0000000000000000000000000000000000000001" } }),
            4901,
        ),
        (
            "params",
            json!({ "type": "ERC20", "options": { "address": "invalid" } }),
            -32602,
        ),
    ] {
        provider
            .request(
                1,
                "doc".into(),
                id.into(),
                "wallet_watchAsset",
                params,
                Instant::now(),
            )
            .unwrap();
        assert_eq!(result(&messages(&mut provider))["error"]["code"], code);
    }
    for index in 0..16 {
        provider
            .request(
                1,
                "doc".into(),
                format!("watch-{index}"),
                "wallet_watchAsset",
                if index % 2 == 0 {
                    params.clone()
                } else {
                    json!([params.clone()])
                },
                Instant::now(),
            )
            .unwrap();
        assert_eq!(result(&messages(&mut provider))["result"], true);
    }
    assert_eq!(provider.approvals.len(), 16);
    personal_approval(&mut provider, "overflow", Instant::now());
    assert_eq!(result(&messages(&mut provider))["error"]["code"], -32005);
    let ready = provider.approval_updates.borrow()[0].clone();
    provider
        .complete_approval(
            &ready.id,
            Err(crate::gateway::GatewayApprovalFailure::Local(
                LocalProviderFailure::UserRejected,
            )),
        )
        .unwrap();
    assert_eq!(provider.approvals.len(), 15);
    assert!(
        messages(&mut provider)
            .iter()
            .all(|(_, message)| message["type"] != "provider_response")
    );
    provider.tick(Instant::now() + APPROVAL_WINDOW);
    assert!(provider.approvals.is_empty());
    assert!(
        messages(&mut provider)
            .iter()
            .all(|(_, message)| message["type"] != "provider_response")
    );
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

mod handoff;

#[tokio::test]
async fn unsealed_remote_outcomes_revalidate_live_authority_before_actor_update() {
    for remote_error in [false, true] {
        for locked in [false, true] {
            let (path, mut provider, view) = fixture();
            let (endpoint, started, release, server) = held_rpc(remote_error).await;
            authorize(&mut provider, &view, endpoint);
            assert!(
                request(
                    &mut provider,
                    1,
                    "doc",
                    "held",
                    "eth_blockNumber",
                    Instant::now()
                )
                .is_empty()
            );
            started.notified().await;
            release.notify_one();
            let completion = provider.jobs.join_next().await.unwrap().unwrap();
            provider.complete_read(completion);
            let (_, mut delivery) = provider.drain().pop().unwrap();
            invalidate_authority(&provider, locked);
            assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Changed);
            let value = serde_json::to_value(&delivery.message).unwrap();
            assert_eq!(value["error"]["code"], if locked { 4100 } else { -32002 });
            assert!(!value.to_string().contains("synthetic-owner-payload"));
            // The replacement is a fixed terminal error and remains deliverable.
            assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Current);
            provider.delivered(delivery.ticket_id().unwrap());
            assert!(provider.reads.is_empty());
            let rejected = request(
                &mut provider,
                1,
                "doc",
                "after-invalidation",
                "eth_blockNumber",
                Instant::now(),
            );
            assert_eq!(
                result(&rejected)["error"]["code"],
                if locked { 4100 } else { -32002 }
            );
            assert!(
                provider.jobs.is_empty(),
                "live invalidation must reject before broker dispatch"
            );
            server.await.unwrap();
            drop(provider);
            drop(view);
            std::fs::remove_dir_all(path).unwrap();
        }
    }
}
