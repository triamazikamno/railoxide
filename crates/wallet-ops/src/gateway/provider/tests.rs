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
        native_currencies: crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .into_values()
        .map(|chain| (chain.chain_id, chain.native_currency))
        .collect(),
        default_chain_id: Some(1),
        ..GatewayWalletState::default()
    }
}
pub(in crate::gateway) fn messages(provider: &mut DappProvider) -> Vec<(u64, Value)> {
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
pub(in crate::gateway) fn snapshot(messages: &[(u64, Value)]) -> &Value {
    &messages
        .iter()
        .find(|(_, message)| message["type"] == "ui_snapshot")
        .unwrap()
        .1
}
fn prompt(messages: &[(u64, Value)]) -> &Value {
    &snapshot(messages)["pending_connects"][0]
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
        let completion = tokio::time::timeout(Duration::from_secs(30), provider.jobs.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        provider.complete_read(completion);
    }
    result(&messages(provider)).clone()
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

fn operation_client() -> crate::OperationHttpClient {
    crate::OperationHttpClient::for_tests(
        reqwest::Client::new(),
        crate::OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
    )
}

mod handoff;

mod approvals;
mod balances;
mod chain_policy;
mod native_review;
mod origin_routes;
mod reads;
mod sessions;
