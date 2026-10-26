use super::*;

pub(super) mod hardware;
mod public_account;
mod swap_fork;
mod swap_observation;
mod swap_order;
mod swap_public;
mod swap_setup;
use crate::{ExecutorDelivery, ExecutorOwner, HttpContext, WalletSyncTip};
use alloy::providers::bindings::IMulticall3;
use alloy::rpc::types::Block;
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::railgun::{
    BoundParams, CommitmentPreimage, RelayAdapt7702, RelayAdapt7702ActionData, SnarkProof,
    Transaction,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, watch};

#[test]
fn executor_spare_migrates_and_is_claimed_once_across_store_handles() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let key = super::super::super::executors::executor_allocation_key(view.wallet_id(), 1);
    let identity = format!("{}:{}:1:{key}", view.wallet_id().len(), view.wallet_id());
    let legacy = rmp_serde::to_vec_named(&json!({"version": 1, "next_index": 999_999})).unwrap();
    let encrypted = view
        .private_view
        .encrypt_record(RecordKind::ExecutorAllocation, &identity, &legacy)
        .unwrap()
        .to_record_entry(key)
        .unwrap();
    db.put_desktop_wallet_vault_records(&[encrypted]).unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let delegate = Address::repeat_byte(2);
    let spare = store.reserve_spare(delegate).unwrap();
    assert_eq!(spare.index(), 999_999);
    store
        .bind_spare_address(spare.index(), Address::repeat_byte(3))
        .unwrap();
    drop(store);
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    assert_eq!(
        store.spare().unwrap().unwrap().address(),
        Some(Address::repeat_byte(3))
    );
    assert_eq!(
        store.reserve_spare(delegate).unwrap().index(),
        spare.index()
    );
    let other = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let allocation_key =
        super::super::super::executors::executor_allocation_key(view.wallet_id(), 1);
    let old_allocation = db
        .get_desktop_wallet_vault_record(&allocation_key)
        .unwrap()
        .unwrap();
    let barrier = std::sync::Barrier::new(2);
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            store
                .reserve(ExecutorOperationId::random().unwrap(), delegate, None, &[])
                .unwrap()
        });
        barrier.wait();
        let second = other
            .reserve(ExecutorOperationId::random().unwrap(), delegate, None, &[])
            .unwrap();
        (first.join().unwrap(), second)
    });
    assert_eq!(
        BTreeSet::from([first.index(), second.index()]),
        BTreeSet::from([999_999, 1_000_064])
    );
    assert!(store.spare().unwrap().is_none());
    db.put_desktop_wallet_vault_records(&[(allocation_key, old_allocation)])
        .unwrap();
    assert!(
        store.spare().unwrap().is_none(),
        "restored allocation cannot resurrect a claimed spare"
    );
    store.retire(first.operation()).unwrap();
    assert_eq!(
        store
            .reserve(first.operation(), delegate, None, &[])
            .unwrap()
            .index(),
        first.index()
    );
    let displaced = store.reserve_spare(delegate).unwrap();
    store
        .bind_spare_address(displaced.index(), Address::repeat_byte(4))
        .unwrap();
    store.raise_floor(displaced.index() + 10).unwrap();
    assert!(store.spare().unwrap().is_none());
    assert!(
        store
            .records()
            .unwrap()
            .iter()
            .any(|record| record.index() == displaced.index() && record.is_retired())
    );
    assert!(store.reserve_spare(delegate).unwrap().index() > displaced.index());
    drop(other);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[derive(Default)]
struct RpcState {
    requests: Mutex<Vec<Value>>,
    /// Each request's `x-route` header, naming the client that sent it, in arrival order.
    routes: Mutex<Vec<Option<String>>>,
    used: Mutex<BTreeSet<Address>>,
    /// Every account counts as used, whatever `used` holds.
    all_used: AtomicBool,
    head: AtomicU64,
    native_balance: AtomicU64,
    changed: Notify,
    /// Response fields replacing the reply to an `eth_call` to an account, such as an empty or
    /// malformed return.
    call_responses: Mutex<std::collections::BTreeMap<Address, Value>>,
    /// Code of the accounts that have any. Every other account has none.
    codes: Mutex<std::collections::BTreeMap<Address, Bytes>>,
    /// The word an `eth_call` to a contract returns, or `None` for a call that fails. Calls to
    /// any other contract return zero.
    calls: Mutex<std::collections::BTreeMap<Address, Option<U256>>>,
    /// A block number that another block now holds, with a different hash.
    reorged: Mutex<Option<u64>>,
    /// The code and execution nonce an account reads at the head block only, as while a
    /// transaction that delegates it is in no confirmed block yet.
    head_accounts: Mutex<std::collections::BTreeMap<Address, (Bytes, U256)>>,
}

struct Rpc {
    url: url::Url,
    state: Arc<RpcState>,
    hold: watch::Sender<Option<Address>>,
    task: tokio::task::JoinHandle<()>,
}

impl Rpc {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let state = Arc::new(RpcState::default());
        state.head.store(10, Ordering::Relaxed);
        let server_state = state.clone();
        let (hold, held) = watch::channel(None);
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        connections.spawn(serve(stream, server_state.clone(), held.clone()));
                    }
                    Some(result) = connections.join_next() => { let _ = result.unwrap(); }
                }
            }
        });
        Self {
            url,
            state,
            hold,
            task,
        }
    }

    fn set_delegated_account(&self, address: Address, delegate: Address, nonce: U256) {
        use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
        self.state.codes.lock().unwrap().insert(
            address,
            [
                EIP7702_DELEGATION_DESIGNATOR.as_slice(),
                delegate.as_slice(),
            ]
            .concat()
            .into(),
        );
        self.state
            .calls
            .lock()
            .unwrap()
            .insert(address, Some(nonce));
    }

    /// `address` reads as delegated to `delegate` with execution nonce `nonce` at the head
    /// block, or with `None` as at every other block.
    fn set_head_account(&self, address: Address, delegated: Option<(Address, U256)>) {
        use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
        let mut accounts = self.state.head_accounts.lock().unwrap();
        if let Some((delegate, nonce)) = delegated {
            let code = [
                EIP7702_DELEGATION_DESIGNATOR.as_slice(),
                delegate.as_slice(),
            ]
            .concat();
            accounts.insert(address, (code.into(), nonce));
        } else {
            accounts.remove(&address);
        }
    }

    fn count_for(&self, address: Address) -> usize {
        self.state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["params"][0] == json!(address))
            .count()
    }

    async fn wait_for(&self, predicate: impl Fn(&[Value]) -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let changed = self.state.changed.notified();
                if predicate(&self.state.requests.lock().unwrap()) {
                    break;
                }
                changed.await;
            }
        })
        .await
        .expect("RPC request should arrive");
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    stream: tokio::net::TcpStream,
    state: Arc<RpcState>,
    mut held: watch::Receiver<Option<Address>>,
) -> std::io::Result<()> {
    let mut stream = BufReader::new(stream);
    let mut content_length = 0;
    let mut route = None;
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if line == "\r\n" {
            break;
        }
        let header = line.to_ascii_lowercase();
        if let Some(length) = header.strip_prefix("content-length:") {
            content_length = length.trim().parse().unwrap();
        }
        if let Some(tag) = header.strip_prefix("x-route:") {
            route = Some(tag.trim().to_owned());
        }
    }
    let mut body = vec![0; content_length];
    stream.read_exact(&mut body).await?;
    let request: Value = serde_json::from_slice(&body).unwrap();
    state.routes.lock().unwrap().push(route);
    state.requests.lock().unwrap().push(request.clone());
    state.changed.notify_one();
    let address = (if request["method"] == "eth_call" {
        &request["params"][0]["to"]
    } else {
        &request["params"][0]
    })
    .as_str()
    .and_then(|value| value.parse::<Address>().ok());
    if (request["method"] == "eth_getCode" || request["method"] == "eth_call") && address.is_some()
    {
        let _ = held.wait_for(|held| *held != address).await;
    }
    let used = state.all_used.load(Ordering::Relaxed)
        || address.is_some_and(|address| state.used.lock().unwrap().contains(&address));
    let called = address
        .filter(|_| request["method"] == "eth_call")
        .and_then(|address| state.calls.lock().unwrap().get(&address).copied());
    let head = state.head.load(Ordering::Relaxed);
    let at_head = request["params"][1] == json!("latest")
        || request["params"][1]["blockHash"] == json!(B256::repeat_byte(head as u8));
    let head_account = address
        .filter(|_| at_head)
        .and_then(|address| state.head_accounts.lock().unwrap().get(&address).cloned());
    let result = match request["method"].as_str().unwrap() {
        "eth_chainId" | "eth_gasPrice" => json!("0x1"),
        "eth_blockNumber" => json!(format!("0x{:x}", state.head.load(Ordering::Relaxed))),
        "eth_getBlockByNumber" => {
            let mut block = Block::<alloy::rpc::types::Transaction>::default();
            block.header.number = request["params"][0]
                .as_str()
                .and_then(|value| value.strip_prefix("0x"))
                .and_then(|number| u64::from_str_radix(number, 16).ok())
                .unwrap_or_else(|| state.head.load(Ordering::Relaxed));
            block.header.hash = if *state.reorged.lock().unwrap() == Some(block.header.number) {
                B256::repeat_byte(0xee)
            } else {
                B256::repeat_byte(block.header.number as u8)
            };
            serde_json::to_value(block).unwrap()
        }
        "eth_getTransactionCount" => json!(if used { "0x1" } else { "0x0" }),
        "eth_getBalance" => json!(format!(
            "0x{:x}",
            state.native_balance.load(Ordering::Relaxed)
        )),
        "eth_getCode" => json!(
            head_account
                .as_ref()
                .map(|(code, _)| code.clone())
                .or_else(|| {
                    address.and_then(|address| state.codes.lock().unwrap().get(&address).cloned())
                })
                .unwrap_or_default()
        ),
        "eth_getStorageAt" => json!(B256::from(U256::from(u8::from(used)))),
        "eth_call" => json!(B256::from(
            head_account
                .as_ref()
                .map(|(_, nonce)| *nonce)
                .or_else(|| called.flatten())
                .unwrap_or_default()
        )),
        "eth_maxPriorityFeePerGas" => json!("0x0"),
        "eth_feeHistory" => json!({
            "oldestBlock": "0x0", "baseFeePerGas": ["0x1", "0x1"],
            "gasUsedRatio": [0.5], "reward": [["0x0"]]
        }),
        "eth_getLogs" => json!([]),
        "eth_estimateGas" => Value::Null,
        method => panic!("unexpected RPC method {method}"),
    };
    let mut response = if request["method"] == "eth_estimateGas" || called == Some(None) {
        json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "test submission rejected"}})
    } else {
        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
    };
    let replaced = address
        .filter(|_| request["method"] == "eth_call")
        .and_then(|address| state.call_responses.lock().unwrap().get(&address).cloned());
    if replaced.is_some() || (request["method"] == "eth_call" && address == Some(multicall())) {
        let response_fields = replaced.unwrap_or_else(|| {
            let input: Bytes = serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
            let call = IMulticall3::tryAggregateCall::abi_decode(&input).unwrap();
            let results = call.calls.iter().map(|_| IMulticall3::Result { success: true, returnData: Bytes::new() }).collect::<Vec<_>>();
            json!({"result": Bytes::from(IMulticall3::tryAggregateCall::abi_encode_returns(&results))})
        });
        response.as_object_mut().unwrap().remove("result");
        response
            .as_object_mut()
            .unwrap()
            .extend(response_fields.as_object().unwrap().clone());
    }
    let response = response.to_string();
    stream.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await
}

fn multicall() -> Address {
    Address::repeat_byte(0xca)
}

fn chain(rpc: &Rpc) -> crate::settings::EffectiveChainConfig {
    let mut chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    chain.finality_depth = 1;
    chain.rpc_route =
        crate::RpcChainRoute::new(1, vec![rpc.url.clone()]).with_multicall(multicall());
    chain
}

fn delivery() -> ExecutorDelivery {
    ExecutorDelivery::SelfBroadcast {
        sender: Address::repeat_byte(8),
        sponsored: false,
    }
}

#[tokio::test]
async fn executor_inspection_is_user_initiated_and_reused_until_handoff() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let owner = Arc::new(
        ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            chain(&rpc),
            HttpContext::direct_for_tests(),
        )
        .unwrap(),
    );
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let (tip, tip_rx) = watch::channel(WalletSyncTip {
        head_block: Some(10),
        ..Default::default()
    });
    owner.start_tip_observation(tip_rx);
    let wrong_password = crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
        "wrong password".into(),
    ));
    assert!(
        owner
            .prepare_operation(
                ExecutorOperationId::random().unwrap(),
                delivery(),
                &wrong_password,
                &[],
                None,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .is_err()
    );
    assert_eq!(store.records().unwrap(), [] as [ExecutorRecord; 0]);
    assert!(rpc.state.requests.lock().unwrap().is_empty());
    let first = owner
        .prepare_operation(
            ExecutorOperationId::random().unwrap(),
            delivery(),
            &crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                TEST_PASSWORD.into(),
            )),
            &[],
            None,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let spare = store.spare().unwrap().unwrap();
    let address = spare.address().unwrap();
    assert_eq!(
        rpc.count_for(address),
        0,
        "preparation must not query the replacement spare"
    );
    let operation = ExecutorOperationId::random().unwrap();
    let authorization = crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
        TEST_PASSWORD.into(),
    ));
    // A preparation cancelled during its inspection leaves nothing pending for the next caller.
    let client = |tag| {
        crate::OperationHttpClient::for_tests(
            crate::rpc_broker::tests::tagged_reqwest_client(tag),
            crate::OperationNetworkIsolation::Dedicated,
        )
    };
    let (first_client, second_client) = (client("first"), client("second"));
    rpc.hold.send_replace(Some(address));
    let mut cancelled = Box::pin(owner.prepare_operation(
        operation,
        delivery(),
        &authorization,
        &[],
        None,
        &first_client,
    ));
    tokio::select! {
        _ = &mut cancelled => panic!("the held inspection must not finish"),
        () = rpc.wait_for(|requests| requests.iter().any(|request| request["params"][0] == json!(address))) => {}
    }
    drop(cancelled);
    rpc.hold.send_replace(None);
    let prepared = owner
        .prepare_operation(
            operation,
            delivery(),
            &authorization,
            &[],
            None,
            &second_client,
        )
        .await
        .unwrap();
    {
        let requests = rpc.state.requests.lock().unwrap();
        let routes = rpc.state.routes.lock().unwrap();
        assert!(
            routes
                .iter()
                .zip(requests.iter())
                .any(|(route, request)| route.as_deref() == Some("second")
                    && request["params"][0] == json!(address)),
            "the next caller reads the account on its own route"
        );
    }
    assert_eq!(prepared.context().executor, address);
    assert_ne!(first.context().executor, address);
    let before = rpc.count_for(address);
    assert!(
        before > 0,
        "claiming the spare for an explicit preparation inspects it"
    );
    assert_eq!(
        rpc.count_for(store.spare().unwrap().unwrap().address().unwrap()),
        0
    );
    let idle_reads = rpc.state.requests.lock().unwrap().len();
    tokio::time::pause();
    tip.send_replace(WalletSyncTip {
        head_block: Some(11),
        ..Default::default()
    });
    tokio::time::advance(Duration::from_mins(5)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        rpc.state.requests.lock().unwrap().len(),
        idle_reads,
        "idle and advancing blocks must not poll unused accounts"
    );
    tokio::time::resume();
    owner
        .prepare_operation(
            operation,
            delivery(),
            &crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                TEST_PASSWORD.into(),
            )),
            &[],
            None,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(
        rpc.count_for(address),
        before,
        "pre-signing retries reuse the same inspection"
    );

    let mut changes = owner.subscribe();
    rpc.state.head.store(9, Ordering::Relaxed);
    tip.send_replace(WalletSyncTip {
        head_block: Some(9),
        ..Default::default()
    });
    tokio::time::timeout(Duration::from_secs(2), changes.changed())
        .await
        .unwrap()
        .unwrap();
    let prepared = owner
        .prepare_operation(
            operation,
            delivery(),
            &crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                TEST_PASSWORD.into(),
            )),
            &[],
            None,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert!(
        rpc.count_for(address) > before,
        "a detected rollback invalidates the unused observation"
    );
    let before_signing = rpc.count_for(address);
    rpc.state.head.store(1000, Ordering::Relaxed);
    tip.send_replace(WalletSyncTip {
        head_block: Some(1000),
        ..Default::default()
    });
    tokio::task::yield_now().await;
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            view.scan_keys().master_public_key,
            Address::repeat_byte(2),
            U256::from(9),
            [7; 16],
        ),
        0,
        0,
        UtxoSource {
            tx_hash: B256::ZERO,
            block_number: 0,
            block_timestamp: 0,
        },
        UtxoCommitmentKind::Shield,
    );
    let call = railgun_wallet::TransactionCall {
        to: address,
        data: RelayAdapt7702::executeCall {
            _transactions: vec![Transaction {
                proof: SnarkProof::default(),
                merkleRoot: B256::ZERO,
                nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
                commitments: Vec::new(),
                boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), address, B256::ZERO),
                unshieldPreimage: CommitmentPreimage::empty(),
            }],
            _actionData: RelayAdapt7702ActionData {
                requireSuccess: true,
                minGasLimit: U256::ZERO,
                calls: Vec::new(),
            },
            _nonce: prepared.context().execution_nonce,
            _signature: Bytes::new(),
        }
        .abi_encode()
        .into(),
    };
    let issued = owner
        .issue_operation(
            &prepared,
            &call,
            std::slice::from_ref(&input),
            &crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                TEST_PASSWORD.into(),
            )),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(
        rpc.count_for(address),
        before_signing,
        "first signing must not repeat the account inspection"
    );
    assert_eq!(
        issued.transaction().authorization_list.as_ref().unwrap()[0]
            .inner()
            .nonce,
        0
    );
    let after_handoff = rpc.state.requests.lock().unwrap().len();
    tokio::time::pause();
    tip.send_replace(WalletSyncTip {
        head_block: Some(1001),
        ..Default::default()
    });
    tokio::time::advance(Duration::from_mins(5)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        rpc.state.requests.lock().unwrap().len(),
        after_handoff,
        "issued work must not start implicit history or account reads"
    );
    tokio::time::resume();
    rpc.state.used.lock().unwrap().insert(address);
    assert!(
        owner
            .issue_operation(
                &prepared,
                &call,
                &[input],
                &crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                    TEST_PASSWORD.into()
                )),
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err(),
        "issued retries must see the changed execution nonce"
    );
    assert_eq!(
        store
            .records()
            .unwrap()
            .iter()
            .find(|record| record.operation() == operation)
            .unwrap()
            .issued()
            .len(),
        1
    );
    owner.shutdown().await;
    let restarted = Arc::new(
        ExecutorOwner::new(
            1,
            db.clone(),
            view.clone(),
            chain(&rpc),
            HttpContext::direct_for_tests(),
        )
        .unwrap(),
    );
    let (_tip, tip_rx) = watch::channel(WalletSyncTip {
        head_block: Some(1001),
        ..Default::default()
    });
    let before_restart = rpc.state.requests.lock().unwrap().len();
    restarted.start_tip_observation(tip_rx);
    tokio::time::pause();
    tokio::time::advance(Duration::from_mins(5)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        rpc.state.requests.lock().unwrap().len(),
        before_restart,
        "restart must not query retained issued accounts or the spare"
    );
    tokio::time::resume();
    let before_check = rpc.count_for(address);
    restarted
        .inspect_record(
            operation,
            &[ExecutorAsset::Native],
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert!(
        rpc.count_for(address) > before_check,
        "an explicit Check still reads its selected account"
    );
    assert_eq!(
        rpc.count_for(store.spare().unwrap().unwrap().address().unwrap()),
        0
    );
    restarted.shutdown().await;
    drop(restarted);
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn paid_fee_retry_reuses_only_its_own_unsent_evidence() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let owner = Arc::new(
        ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            chain(&rpc),
            HttpContext::direct_for_tests(),
        )
        .unwrap(),
    );
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let password = || {
        crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(TEST_PASSWORD.into()))
    };
    let input = |position| {
        Utxo::new(
            broadcaster_core::notes::Note::new_change(
                view.scan_keys().master_public_key,
                Address::repeat_byte(2),
                U256::from(9),
                [7; 16],
            ),
            0,
            position,
            UtxoSource {
                tx_hash: B256::ZERO,
                block_number: 0,
                block_timestamp: 0,
            },
            UtxoCommitmentKind::Shield,
        )
    };
    // Each fee round proves a different call under the same execution nonce.
    let call = |prepared: &crate::PreparedExecutorOperation, input: &Utxo, round: usize| {
        let executor = prepared.context().executor;
        railgun_wallet::TransactionCall {
            to: executor,
            data: RelayAdapt7702::executeCall {
                _transactions: vec![Transaction {
                    proof: SnarkProof::default(),
                    merkleRoot: B256::ZERO,
                    nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
                    commitments: vec![B256::repeat_byte(8); round],
                    boundParams: BoundParams::new_transact(
                        0,
                        0,
                        1,
                        Vec::new(),
                        executor,
                        B256::ZERO,
                    ),
                    unshieldPreimage: CommitmentPreimage::empty(),
                }],
                _actionData: RelayAdapt7702ActionData {
                    requireSuccess: true,
                    minGasLimit: U256::ZERO,
                    calls: Vec::new(),
                },
                _nonce: prepared.context().execution_nonce,
                _signature: Bytes::new(),
            }
            .abi_encode()
            .into(),
        }
    };
    let requests = || rpc.state.requests.lock().unwrap().len();
    let record = |operation| {
        store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    };

    let operation = ExecutorOperationId::random().unwrap();
    let prepared = owner
        .prepare_operation(
            operation,
            delivery(),
            &password(),
            &[],
            None,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let spent = input(0);
    let mut retry = crate::desktop::executors::IssueRetry::default();
    let first = owner
        .issue_operation_retrying(
            &prepared,
            &call(&prepared, &spent, 0),
            std::slice::from_ref(&spent),
            &password(),
            &mut retry,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let before = requests();
    let repriced = call(&prepared, &spent, 1);
    let revised = owner
        .issue_operation_retrying(
            &prepared,
            &repriced,
            std::slice::from_ref(&spent),
            &password(),
            &mut retry,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(requests(), before, "a repriced round reuses its evidence");
    assert_ne!(revised.payload_hash(), first.payload_hash());
    let signed =
        RelayAdapt7702::executeCall::abi_decode(revised.transaction().input.input().unwrap())
            .unwrap();
    assert_eq!(
        alloy::primitives::Signature::try_from(signed._signature.as_ref())
            .unwrap()
            .recover_address_from_prehash(&prepared.context().signing_hash(&repriced).unwrap())
            .unwrap(),
        prepared.context().executor
    );
    let saved = record(operation);
    assert_eq!(saved.issued().len(), 2);
    assert!(saved.issued().iter().any(|payload| {
        payload.hash() == revised.payload_hash()
            && payload.context().calldata() == revised.transaction().input.input().unwrap()
    }));
    assert!(
        saved
            .reserved_inputs()
            .iter()
            .any(|reserved| reserved.matches(&spent))
    );
    // A payload sent under this context may have executed; recheck the chain.
    store
        .record_submission(operation, first.payload_hash(), B256::repeat_byte(0x77))
        .unwrap();
    let before = requests();
    owner
        .issue_operation_retrying(
            &prepared,
            &call(&prepared, &spent, 2),
            std::slice::from_ref(&spent),
            &password(),
            &mut retry,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert!(
        requests() > before,
        "a sent payload requires fresh evidence"
    );
    {
        let seen = rpc.state.requests.lock().unwrap();
        let round = &seen[before..];
        assert!(
            round
                .iter()
                .any(|request| request["method"] == "eth_getCode"),
            "a retry with issued payloads reads the account's delegation"
        );
        assert!(
            round
                .iter()
                .all(|request| request["method"] != "eth_getLogs"
                    && (request["method"] != "eth_getBlockByNumber"
                        || request["params"][1] != json!(true))),
            "a retry reads no block history"
        );
    }

    // History reconciliation may drop the durable observation between rounds.
    let operation = ExecutorOperationId::random().unwrap();
    let prepared = owner
        .prepare_operation(
            operation,
            delivery(),
            &password(),
            &[],
            None,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let spent = input(1);
    let mut retry = crate::desktop::executors::IssueRetry::default();
    owner
        .issue_operation_retrying(
            &prepared,
            &call(&prepared, &spent, 0),
            std::slice::from_ref(&spent),
            &password(),
            &mut retry,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    store.invalidate_observation(operation).unwrap();
    let before = requests();
    owner
        .issue_operation_retrying(
            &prepared,
            &call(&prepared, &spent, 1),
            std::slice::from_ref(&spent),
            &password(),
            &mut retry,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert!(
        requests() > before,
        "an invalidated observation requires fresh evidence"
    );
    assert_eq!(record(operation).issued().len(), 2);

    // The retry's account read is taken at the inspection's confirmed block. Once another
    // block holds that height, the inspection's nonce is not reused and nothing is signed.
    store.invalidate_observation(operation).unwrap();
    rpc.hold.send_replace(Some(prepared.context().executor));
    let before = requests();
    let reorged_call = call(&prepared, &spent, 2);
    let authorization = password();
    let error = {
        let pending = owner.issue_operation_retrying(
            &prepared,
            &reorged_call,
            std::slice::from_ref(&spent),
            &authorization,
            &mut retry,
            crate::OperationHttpClient::direct_for_tests(),
        );
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => panic!("signing did not inspect the account: {}", result.is_ok()),
            () = rpc.wait_for(|requests| requests[before..].iter().any(|request| request["method"] == "eth_getCode")) => {}
        }
        // The inspection already holds its confirmed block, at the head minus the finality depth.
        *rpc.state.reorged.lock().unwrap() = Some(rpc.state.head.load(Ordering::Relaxed) - 1);
        rpc.hold.send_replace(None);
        pending
            .await
            .err()
            .expect("a replaced signing block is refused")
    };
    assert!(
        format!("{error:#}").contains("no longer canonical"),
        "{error:#}"
    );
    assert_eq!(record(operation).issued().len(), 2);
    assert!(record(operation).nonce_observation().is_none());
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn executor_spare_restart_stays_idle_until_preparation_and_shutdown_cancels_it() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let spare = store
        .reserve_spare(chain.accepted_executor_profile().unwrap().delegate())
        .unwrap();
    let (_, signer) = vault
        .executor_spend_signers_for_session(
            &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
            &view,
            None,
            1,
            spare.index(),
        )
        .unwrap();
    let address = signer.address();
    drop(signer);
    store.bind_spare_address(spare.index(), address).unwrap();
    rpc.hold.send_replace(Some(address));
    let owner = Arc::new(
        ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            chain.clone(),
            HttpContext::direct_for_tests(),
        )
        .unwrap(),
    );
    let (_tip, tip_rx) = watch::channel(WalletSyncTip::default());
    owner.start_tip_observation(tip_rx);
    tokio::time::pause();
    tokio::time::advance(Duration::from_mins(5)).await;
    tokio::task::yield_now().await;
    assert!(
        rpc.state.requests.lock().unwrap().is_empty(),
        "restart must not prefetch the retained spare"
    );
    tokio::time::resume();
    let operation = ExecutorOperationId::random().unwrap();
    {
        let authorization = crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
            TEST_PASSWORD.into(),
        ));
        let prepare = owner.prepare_operation(
            operation,
            delivery(),
            &authorization,
            &[ExecutorAsset::Native],
            Some("Unshield 0.5 ETH"),
            crate::OperationHttpClient::direct_for_tests(),
        );
        tokio::pin!(prepare);
        tokio::select! {
            result = &mut prepare => panic!("blocked inspection unexpectedly completed: {}", result.is_ok()),
            () = rpc.wait_for(|requests| requests.iter().any(|request| request["method"] == "eth_getCode")) => {}
        }
        assert_eq!(
            rpc.state
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == "eth_getCode")
                .count(),
            1,
            "the user preparation starts one account inspection"
        );
        tokio::time::timeout(Duration::from_secs(2), owner.shutdown())
            .await
            .expect("shutdown cancels the inspection without waiting for RPC");
        assert!(prepare.await.is_err());
    }
    assert_eq!(store.records().unwrap()[0].address(), Some(address));
    let retained = store.records().unwrap().remove(0);
    assert!(retained.created_at().is_some());
    assert_eq!(retained.purpose_summary(), Some("Unshield 0.5 ETH"));
    assert_eq!(retained.assets(), &[ExecutorAsset::Native]);
    assert!(store.spare().unwrap().unwrap().index() > spare.index());
    rpc.hold.send_replace(None);
    let replacement = ExecutorOwner::new(
        1,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let prepared = replacement
        .prepare_operation(
            operation,
            delivery(),
            &crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                TEST_PASSWORD.into(),
            )),
            &[],
            None,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(prepared.context().executor, address);
    let retried = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(retried.created_at(), retained.created_at());
    assert_eq!(retried.purpose_summary(), retained.purpose_summary());
    assert_eq!(retried.assets(), retained.assets());
    assert!(store.spare().unwrap().unwrap().index() > spare.index());
    replacement.shutdown().await;
    drop(replacement);
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn hardware_stopping_restore_keeps_the_derived_range_without_applying_a_late_reply() {
    for descriptor in [
        None,
        Some(test_hardware_descriptor(0)),
        Some(test_trezor_hardware_descriptor(0)),
    ] {
        let rpc = Rpc::start().await;
        let (root, db, vault) = desktop_store_with_vault();
        let view = match descriptor.as_ref() {
            Some(descriptor) => hardware::hardware_view(&vault, descriptor),
            None => Arc::new(import_wallet_with_metadata(
                &vault,
                TEST_WALLET_ID,
                "Wallet",
            )),
        };
        let records = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
        let owner = Arc::new(
            ExecutorOwner::new(
                0,
                db.clone(),
                view.clone(),
                chain(&rpc),
                HttpContext::direct_for_tests(),
            )
            .unwrap(),
        );
        let pending = {
            let authorization = match descriptor.as_ref() {
                Some(descriptor) => hardware::authorize(
                    &owner,
                    &view,
                    descriptor,
                    crate::HardwareExecutorAction::Restore(10..12),
                ),
                None => crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                    TEST_PASSWORD.into(),
                )),
            };
            let restore = owner.discover_range(&authorization, 10..12);
            tokio::pin!(restore);
            let mut changes = owner.subscribe();
            let checked = |record: &ExecutorRecord| record.use_check().observation().is_some();
            while !records.records().unwrap().iter().any(checked) {
                tokio::select! {
                    result = &mut restore => panic!("restore should still be checking: {}", result.is_ok()),
                    changed = changes.changed() => changed.unwrap(),
                }
            }
            // The future is not polled here, so the other address's check has not started.
            let pending = records
                .records()
                .unwrap()
                .into_iter()
                .find(|record| !checked(record))
                .unwrap()
                .address()
                .unwrap();
            rpc.hold.send_replace(Some(pending));
            tokio::select! {
                result = &mut restore => panic!("the second check should be blocked: {}", result.is_ok()),
                () = rpc.wait_for(|requests| requests.iter().any(|request| {
                    request["method"] == "eth_call" && request["params"][0]["to"] == json!(pending)
                })) => {},
            }
            // Dropping the future is the same cancellation boundary as the panel's Stop.
            pending
        };
        rpc.hold.send_replace(None);
        owner.shutdown().await;
        let retained = records.records().unwrap();
        assert_eq!(
            retained
                .iter()
                .map(ExecutorRecord::index)
                .collect::<Vec<_>>(),
            vec![10, 11]
        );
        assert!(
            retained
                .iter()
                .all(|record| record.address().is_some() && record.restored_at().is_some())
        );
        // The result recorded before Stop remains; the late reply is not applied.
        let (checked, late): (Vec<_>, Vec<_>) = retained
            .iter()
            .partition(|record| record.use_check().observation().is_some());
        assert_eq!(checked.len(), 1);
        assert_eq!(late[0].address(), Some(pending));
        assert!(!late[0].use_check().is_unavailable());
        drop(owner);
        drop(records);
        drop(view);
        drop(vault);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn hardware_restore_checks_64_addresses_on_own_routes_and_keeps_success_after_failure_and_restart()
 {
    // Building a client loads TLS roots, so each tagged client is built once and reused by
    // every pass and setup. Within one pass, every open still gets a distinct client.
    let shared = crate::rpc_broker::tests::tagged_reqwest_client("shared");
    let route_clients = Arc::new(
        (0..64)
            .map(|index| {
                crate::rpc_broker::tests::tagged_reqwest_client(Box::leak(
                    format!("route-{index}").into_boxed_str(),
                ))
            })
            .collect::<Vec<_>>(),
    );
    for descriptor in [
        None,
        Some(test_hardware_descriptor(0)),
        Some(test_trezor_hardware_descriptor(0)),
    ] {
        let rpc = Rpc::start().await;
        // Each opened client tags its requests with a route of its own. The eighth fails to open.
        let opened = AtomicUsize::new(0);
        let route_clients = route_clients.clone();
        let http = HttpContext::with_rpc_client_for_tests(
            shared.clone(),
            crate::WalletNetworkMode::Direct,
        )
        .with_operation_clients_for_tests(move || {
            let count = opened.fetch_add(1, Ordering::Relaxed);
            if count == 7 {
                return Err(eyre::eyre!("route unavailable"));
            }
            Ok(crate::OperationHttpClient::for_tests(
                route_clients[count % route_clients.len()].clone(),
                crate::OperationNetworkIsolation::Dedicated,
            ))
        });
        let (root, db, vault) = desktop_store_with_vault();
        let view = match descriptor.as_ref() {
            Some(descriptor) => hardware::hardware_view(&vault, descriptor),
            None => Arc::new(import_wallet_with_metadata(
                &vault,
                TEST_WALLET_ID,
                "Wallet",
            )),
        };
        let owner =
            Arc::new(ExecutorOwner::new(0, db.clone(), view.clone(), chain(&rpc), http).unwrap());
        let authorization = || match descriptor.as_ref() {
            Some(descriptor) => hardware::authorize(
                &owner,
                &view,
                descriptor,
                crate::HardwareExecutorAction::Restore(0..64),
            ),
            None => crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                TEST_PASSWORD.into(),
            )),
        };
        // Every account reads a zero nonce, which still counts as Used.
        let report = owner.discover_range(&authorization(), 0..64).await.unwrap();
        assert_eq!(
            (report.used(), report.unused(), report.unavailable()),
            (63, 0, 1)
        );
        let saved = owner.records().unwrap();
        let addresses = saved
            .iter()
            .map(|record| record.address().unwrap())
            .collect::<Vec<_>>();
        let failed = saved
            .iter()
            .position(|record| record.use_check().is_unavailable())
            .unwrap();
        assert!(saved[failed].use_check().observation().is_none());
        {
            let requests = rpc.state.requests.lock().unwrap();
            let routes = rpc.state.routes.lock().unwrap();
            let mut checks = std::collections::BTreeMap::<&str, Vec<&Value>>::new();
            for (route, request) in routes.iter().zip(requests.iter()) {
                let route = route.as_deref().unwrap();
                assert!(
                    route.starts_with("route-"),
                    "the shared client sends no check"
                );
                checks.entry(route).or_default().push(request);
            }
            let mut checked = BTreeSet::new();
            for requests in checks.values() {
                assert_eq!(
                    requests
                        .iter()
                        .map(|request| request["method"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    vec!["eth_chainId", "eth_getBlockByNumber", "eth_call"],
                    "each route checks its chain before the address leaves, and Restore must not \
                     query balances, account nonces, code, storage or history"
                );
                let params = &requests[2]["params"];
                let input: Bytes = serde_json::from_value(params[0]["input"].clone()).unwrap();
                assert_eq!(input, RelayAdapt7702::nonceCall {}.abi_encode());
                assert_eq!(
                    params[1],
                    json!({"blockHash": B256::repeat_byte(10), "requireCanonical": true})
                );
                assert!(
                    checked.insert(
                        serde_json::from_value::<Address>(params[0]["to"].clone()).unwrap()
                    ),
                    "no address is checked twice"
                );
            }
            // An address whose route can't open is sent nowhere else.
            let expected = addresses
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != failed)
                .map(|(_, address)| *address)
                .collect::<BTreeSet<_>>();
            assert_eq!(checked, expected);
        }
        // A nonce, including zero, is Used and an empty return Unused. A revert or a
        // malformed reply leaves the account unavailable.
        rpc.state
            .calls
            .lock()
            .unwrap()
            .extend([(addresses[0], Some(U256::ONE)), (addresses[3], None)]);
        rpc.state
            .call_responses
            .lock()
            .unwrap()
            .extend(
                addresses
                    .iter()
                    .enumerate()
                    .filter_map(|(index, address)| match index {
                        0 | 1 | 3 => None,
                        4 => Some((*address, json!({"result": Bytes::from_static(&[1])}))),
                        _ => Some((*address, json!({"result": Bytes::new()}))),
                    }),
            );
        let report = owner.discover_range(&authorization(), 0..64).await.unwrap();
        assert_eq!(
            (report.used(), report.unused(), report.unavailable()),
            (2, 60, 2)
        );
        let saved = owner.records().unwrap();
        assert!(saved[0].use_check().observation().unwrap().was_used());
        assert!(
            saved[1].use_check().observation().unwrap().was_used(),
            "zero nonce is still delegated"
        );
        assert!(!saved[2].use_check().observation().unwrap().was_used());
        assert!(saved[3].use_check().is_unavailable() && saved[4].use_check().is_unavailable());
        rpc.state.call_responses.lock().unwrap().clear();
        rpc.state
            .calls
            .lock()
            .unwrap()
            .extend(addresses.iter().map(|address| (*address, None)));
        let reads = rpc.state.requests.lock().unwrap().len();
        let report = owner.discover_range(&authorization(), 0..64).await.unwrap();
        assert_eq!(
            (report.used(), report.unused(), report.unavailable()),
            (0, 0, 64)
        );
        assert_eq!(
            rpc.state.requests.lock().unwrap().len(),
            reads + 3 * 64,
            "a failed check must not fan out"
        );
        owner.shutdown().await;
        drop(owner);
        let restarted = ExecutorOwner::new(
            1,
            db.clone(),
            view.clone(),
            chain(&rpc),
            HttpContext::direct_for_tests(),
        )
        .unwrap();
        let reloaded = restarted.records().unwrap();
        for (before, after) in saved.iter().zip(&reloaded) {
            assert_eq!(
                before.use_check().observation(),
                after.use_check().observation()
            );
            assert!(after.use_check().is_unavailable());
            assert_eq!(after.assets(), [] as [ExecutorAsset; 0]);
            assert_eq!(before.operation(), after.operation());
        }
        restarted.shutdown().await;
        drop(restarted);
        drop(view);
        drop(vault);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}

// Each address is checked on a route of its own, but endpoint health is the broker's: an endpoint
// that failed one address's check sits out the next address's. A check still reaches an endpoint
// whose identity check finished only after the admitted one failed.
#[tokio::test]
async fn restore_checks_skip_an_endpoint_that_failed_and_reach_one_admitted_late() {
    let mut block = Block::<alloy::rpc::types::Transaction>::default();
    block.header.number = 10;
    block.header.hash = B256::repeat_byte(10);
    let block = serde_json::to_value(block).unwrap();
    // It passes its identity check, then fails every other request at the endpoint.
    let (failing, failing_requests, failing_task) =
        crate::rpc_broker::tests::spawn_recording_rpc_mock(Arc::new(|request: Value| {
            if request["method"] == "eth_chainId" {
                json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x1"})
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {"code": -32005, "message": "limit exceeded"}
                })
            }
        }))
        .await;
    // It holds every request until released, so its identity check is still pending when the
    // first check starts.
    let gate = crate::rpc_broker::tests::RpcMockGate::default();
    let (serving, serving_task) = crate::rpc_broker::tests::spawn_gated_rpc_mock(
        Arc::new(move |request: Value| {
            let result = match request["method"].as_str().unwrap() {
                "eth_chainId" => json!("0x1"),
                "eth_getBlockByNumber" => block.clone(),
                // An empty return: the account is unused.
                "eth_call" => json!("0x"),
                method => panic!("unexpected RPC method {method}"),
            };
            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        }),
        Arc::default(),
        Arc::default(),
        gate.clone(),
    )
    .await;
    let data_requests = move || {
        failing_requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, request)| request["method"] != "eth_chainId")
            .count()
    };
    // The serving endpoint answers once the failing one was sent a check's read.
    let releaser = {
        let data_requests = data_requests.clone();
        tokio::spawn(async move {
            while data_requests() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            loop {
                gate.release_response.notify_waiters();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let mut chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    chain.finality_depth = 1;
    chain.rpc_route =
        crate::RpcChainRoute::new(1, vec![failing, serving]).with_identity_verification();
    let http = HttpContext::direct_for_tests();
    let owner =
        ExecutorOwner::new(0, db.clone(), view.clone(), chain.clone(), http.clone()).unwrap();
    let authorization = crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
        TEST_PASSWORD.into(),
    ));

    let report = owner.discover_range(&authorization, 0..1).await.unwrap();
    assert_eq!(
        (report.used(), report.unused(), report.unavailable()),
        (0, 1, 0)
    );
    assert_eq!(data_requests(), 1);
    // The strike reaches the broker on its advisory channel.
    let route = chain
        .rpc_route
        .for_class(crate::RpcTrafficClass::Sensitive)
        .unwrap();
    let serving = &route.endpoints()[1..];
    tokio::time::timeout(Duration::from_secs(5), async {
        while http
            .rpc_broker()
            .endpoint_candidates(route.route(), None)
            .await
            .unwrap()
            != serving
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let report = owner.discover_range(&authorization, 1..2).await.unwrap();
    assert_eq!(
        (report.used(), report.unused(), report.unavailable()),
        (0, 1, 0)
    );
    assert_eq!(data_requests(), 1, "the next address's check skips it");

    owner.shutdown().await;
    for task in [releaser, failing_task, serving_task] {
        task.abort();
    }
    drop(owner);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn background_confirmation_resolves_from_private_sync_without_reading_the_chain() {
    use railgun_wallet::WalletUtxo;
    use sync_service::{WalletCurrentSnapshot, WalletPendingOverlay};

    /// One pass of the observer over the only record. `current` is the private snapshot by
    /// the time the pass writes.
    async fn pass(
        owner: &ExecutorOwner,
        seen: &Arc<WalletCurrentSnapshot>,
        current: &Arc<WalletCurrentSnapshot>,
        safe_head: Option<u64>,
    ) -> (bool, ExecutorRecord) {
        let record = owner.records().unwrap().remove(0);
        let kept = owner
            .confirm_synced_record(seen, &|| Some(current.clone()), safe_head, record)
            .await
            .unwrap();
        (kept, owner.records().unwrap().remove(0))
    }

    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain(&rpc),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let delegate = Address::repeat_byte(1);
    store.reserve(operation, delegate, None, &[]).unwrap();
    store
        .bind_address(operation, Address::repeat_byte(3))
        .unwrap();
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(U256::ONE, Address::ZERO, U256::from(9), [7; 16]),
        2,
        3,
        UtxoSource {
            tx_hash: B256::repeat_byte(9),
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    );
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::from(3));
    store.record_account_read(operation, observed).unwrap();
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::from(3),
                delegate,
                B256::repeat_byte(4),
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(
                    Bytes::from_static(b"operation"),
                    observed,
                    vec![ExecutorInputIdentity::from_utxo(&input)],
                ),
            ),
        )
        .unwrap();
    store
        .record_submission(operation, B256::repeat_byte(4), B256::repeat_byte(5))
        .unwrap();
    // Private sync's view of the payload's note, spent in block 14 by `transaction` if any.
    let snapshot = |reset_generation: u64, transaction: Option<u8>| {
        let mut note = WalletUtxo::new(input.clone());
        note.spent = transaction.map(|transaction| UtxoSource {
            tx_hash: B256::repeat_byte(transaction),
            block_number: 14,
            block_timestamp: 1,
        });
        WalletCurrentSnapshot::new(
            20,
            0,
            reset_generation,
            vec![note],
            WalletPendingOverlay::default(),
        )
    };

    // Startup with a pending payload: private sync is past the last read, which settles the
    // nonces below the payload's. The payload stays pending and reserved, also as the tip
    // advances.
    let unspent = snapshot(0, None);
    let (_, pending) = pass(&owner, &unspent, &unspent, None).await;
    assert_eq!(pending.settled_nonce(), Some(U256::from(3)));
    assert_eq!(
        pending.payload_state(B256::repeat_byte(4)),
        Some(ExecutorPayloadState::Pending)
    );
    assert_eq!(pending.reserved_inputs().len(), 1);
    for safe_head in [Some(15), Some(16)] {
        assert_eq!(pass(&owner, &unspent, &unspent, safe_head).await.1, pending);
    }
    // A spend in a transaction that is not recorded for the payload, a recorded spend above
    // the safe head, and a snapshot that private sync reset before the write change nothing.
    let unrelated = snapshot(0, Some(6));
    assert_eq!(pass(&owner, &unrelated, &unrelated, None).await.1, pending);
    let spent = snapshot(0, Some(5));
    assert_eq!(pass(&owner, &spent, &spent, Some(13)).await.1, pending);
    assert_eq!(
        pass(&owner, &spent, &snapshot(1, None), Some(14)).await,
        (false, pending.clone())
    );

    // The recorded spend at the safe head resolves the payload and settles its nonce at
    // once. It is no account read, so the nonce observation stays.
    let (_, resolved) = pass(&owner, &spent, &spent, Some(14)).await;
    assert_eq!(
        resolved.nonce_watermark(),
        Some(ExecutorNonceWatermark::new(U256::from(4), 14))
    );
    assert_eq!(resolved.settled_nonce(), Some(U256::from(4)));
    assert_eq!(resolved.nonce_observation(), pending.nonce_observation());
    assert_eq!(resolved.reserved_inputs(), [] as [ExecutorInputIdentity; 0]);
    assert!(rpc.state.requests.lock().unwrap().is_empty());
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// A broadcaster's reply was lost, so the operation has no recorded transaction and private
// sync cannot confirm it. After a restart Check balance reads the account's execution nonce
// at the confirmed block, which resolves it without reading a block, a receipt or a log.
#[tokio::test]
async fn check_balance_resolves_a_lost_reply_operation_after_a_restart() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let delegate = chain.accepted_executor_profile().unwrap().delegate();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let executor = Address::repeat_byte(3);
    store.reserve(operation, delegate, None, &[]).unwrap();
    store.bind_address(operation, executor).unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(5, B256::repeat_byte(5)), U256::ZERO);
    store.record_account_read(operation, observed).unwrap();
    let payload = B256::repeat_byte(4);
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                delegate,
                payload,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"operation"), observed, Vec::new()),
            ),
        )
        .unwrap();

    // The owner of the session after the restart. Its status reads no chain state.
    let owner = ExecutorOwner::new(
        1,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let record = || owner.records().unwrap().remove(0);
    let before = owner.account_status(&record()).unwrap();
    assert_eq!(before.outcome(), crate::ExecutorAccountOutcome::Unconfirmed);
    assert!(before.read_this_session().is_none());
    assert!(rpc.state.requests.lock().unwrap().is_empty());

    // The operation ran: the account is delegated and its nonce is past the payload's.
    rpc.set_delegated_account(executor, delegate, U256::ONE);
    rpc.state.head.store(30, Ordering::Relaxed);
    owner
        .check_record(
            operation,
            &[ExecutorAsset::Native],
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let after = owner.account_status(&record()).unwrap();
    assert_eq!(after.outcome(), crate::ExecutorAccountOutcome::Executed);
    assert_eq!(
        record().payload_state(payload),
        Some(ExecutorPayloadState::Resolved)
    );
    // The nonce is read at the confirmed block, one below the head on this chain.
    let confirmed = BlockNumHash::new(29, B256::repeat_byte(29));
    assert_eq!(after.read_this_session(), Some(confirmed));
    assert_eq!(record().nonce_observation().unwrap().block(), confirmed);
    assert!(rpc.state.requests.lock().unwrap().iter().all(|request| {
        request["method"] != "eth_getLogs"
            && (request["method"] != "eth_getBlockByNumber" || request["params"][1] != json!(true))
    }));
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn native_shield_issues_a_delegated_operation_without_inputs_and_records_its_handoff() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let owner = Arc::new(
        ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            chain.clone(),
            HttpContext::direct_for_tests(),
        )
        .unwrap(),
    );
    let (_tip, tip_rx) = watch::channel(WalletSyncTip {
        head_block: Some(10),
        ..Default::default()
    });
    owner.start_tip_observation(tip_rx);
    let wrapped_native = chain.wrapped_native_token.unwrap();
    let recipient = crate::public_wallet::public_shield_recipient(&view).unwrap();
    let request = broadcaster_core::contracts::shield::build_shield_request(
        recipient.master_public_key,
        &recipient.viewing_public_key,
        broadcaster_core::contracts::railgun::TokenData::erc20(wrapped_native),
        alloy::primitives::aliases::U120::from(5_000_u64),
        &[3; 32],
    )
    .unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let payer = Address::repeat_byte(8);
    let amount = U256::from(5_000_u64);
    let issued = owner
        .issue_native_shield(
            operation,
            &view,
            payer,
            &crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                TEST_PASSWORD.into(),
            )),
            wrapped_native,
            amount,
            request.clone(),
            "Shield",
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let record = || {
        owner
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    };
    let account = record().address().unwrap();

    // The payer sends the amount to the stealth account, which delegates itself.
    let transaction = issued.transaction();
    assert_eq!(
        (transaction.from, transaction.to, transaction.value),
        (
            Some(payer),
            Some(alloy::primitives::TxKind::Call(account)),
            Some(amount)
        )
    );
    assert_eq!(transaction.transaction_type, Some(4));
    let [authorization] = transaction.authorization_list.as_deref().unwrap() else {
        panic!("one delegation");
    };
    assert_eq!(
        *authorization.address(),
        chain.accepted_executor_profile().unwrap().delegate()
    );
    assert_eq!(authorization.recover_authority().unwrap(), account);
    let call = RelayAdapt7702::executeCall::abi_decode(transaction.input.input().unwrap()).unwrap();
    assert!(call._transactions.is_empty());

    // The durable payload is an ordinary operation that spends no private inputs.
    let issued_record = record();
    let [payload] = issued_record.issued() else {
        panic!("one payload");
    };
    assert_eq!(payload.hash(), issued.payload_hash());
    assert_eq!(
        payload.purpose(),
        crate::vault::ExecutorPayloadPurpose::Operation
    );
    assert_eq!(payload.context().inputs(), [] as [ExecutorInputIdentity; 0]);
    assert_eq!(payload.transaction_hashes(), [] as [B256; 0]);

    let sent = B256::repeat_byte(9);
    let mut handoff = owner.native_shield_handoff(&issued).unwrap();
    handoff(sent, transaction, &[]).unwrap();
    drop(handoff);
    assert_eq!(record().issued()[0].transaction_hashes(), [sent]);

    // The attempt was included and reverted: the account is now delegated and its
    // authorization nonce consumed, while its execution nonce stays unresolved.
    let delegate = chain.accepted_executor_profile().unwrap().delegate();
    rpc.state.used.lock().unwrap().insert(account);
    rpc.set_delegated_account(account, delegate, U256::ZERO);
    let retried = owner
        .issue_native_shield(
            operation,
            &view,
            payer,
            &crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                TEST_PASSWORD.into(),
            )),
            wrapped_native,
            amount,
            request,
            "Shield",
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    // The retry still delegates the same account, at its next authorization nonce.
    let transaction = retried.transaction();
    assert_eq!(
        transaction.to,
        Some(alloy::primitives::TxKind::Call(account))
    );
    assert_eq!(transaction.transaction_type, Some(4));
    let [authorization] = transaction.authorization_list.as_deref().unwrap() else {
        panic!("one delegation");
    };
    assert_eq!(authorization.recover_authority().unwrap(), account);
    assert_eq!(*authorization.address(), delegate);
    assert_eq!(authorization.nonce(), 1);

    owner.shutdown().await;
    drop(owner);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
