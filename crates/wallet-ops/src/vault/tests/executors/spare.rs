use super::*;

mod public_account;
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
    atomic::{AtomicU64, Ordering},
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
    used: Mutex<BTreeSet<Address>>,
    head: AtomicU64,
    changed: Notify,
    aggregate_response: Mutex<Option<Value>>,
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
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if line == "\r\n" {
            break;
        }
        if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = length.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; content_length];
    stream.read_exact(&mut body).await?;
    let request: Value = serde_json::from_slice(&body).unwrap();
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
    let used = address.is_some_and(|address| state.used.lock().unwrap().contains(&address));
    let result = match request["method"].as_str().unwrap() {
        "eth_chainId" => json!("0x1"),
        "eth_blockNumber" => json!(format!("0x{:x}", state.head.load(Ordering::Relaxed))),
        "eth_getBlockByNumber" => {
            let mut block = Block::<alloy::rpc::types::Transaction>::default();
            block.header.number = request["params"][0]
                .as_str()
                .and_then(|value| value.strip_prefix("0x"))
                .and_then(|number| u64::from_str_radix(number, 16).ok())
                .unwrap_or_else(|| state.head.load(Ordering::Relaxed));
            block.header.hash = B256::repeat_byte(block.header.number as u8);
            serde_json::to_value(block).unwrap()
        }
        "eth_getTransactionCount" => json!(if used { "0x1" } else { "0x0" }),
        "eth_getBalance" => json!("0x0"),
        "eth_getCode" => json!("0x"),
        "eth_getStorageAt" => json!(B256::from(U256::from(u8::from(used)))),
        "eth_call" => json!(B256::ZERO),
        method => panic!("unexpected RPC method {method}"),
    };
    let mut response = json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
    if request["method"] == "eth_call" && address == Some(multicall()) {
        let response_fields = state.aggregate_response.lock().unwrap().clone().unwrap_or_else(|| {
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
    let first = owner
        .prepare_operation(
            ExecutorOperationId::random().unwrap(),
            delivery(),
            &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
            None,
            &[],
            None,
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
    let prepared = owner
        .prepare_operation(
            operation,
            delivery(),
            &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
            None,
            &[],
            None,
        )
        .await
        .unwrap();
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
            &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
            None,
            &[],
            None,
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
            &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
            None,
            &[],
            None,
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
            &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
            None,
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
    assert_eq!(
        store
            .records()
            .unwrap()
            .iter()
            .find(|record| record.operation() == operation)
            .unwrap()
            .issued()[0]
            .context()
            .history_start(),
        999
    );
    rpc.state.used.lock().unwrap().insert(address);
    assert!(
        owner
            .issue_operation(
                &prepared,
                &call,
                &[input],
                &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
                None
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
        .inspect_record(operation, &[ExecutorAsset::Native])
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
        let mut grant = vault.create_spend_grant(TEST_PASSWORD).unwrap();
        let prepare = owner.prepare_operation(
            operation,
            delivery(),
            &mut grant,
            None,
            &[ExecutorAsset::Native],
            Some("Unshield 0.5 ETH"),
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
            &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
            None,
            &[],
            None,
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
async fn stopping_restore_keeps_the_derived_range_without_applying_a_late_reply() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let records = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    rpc.hold.send_replace(Some(multicall()));
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain(&rpc),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    {
        let mut grant = vault.create_spend_grant(TEST_PASSWORD).unwrap();
        let restore = owner.discover_range(&mut grant, None, 10..12);
        tokio::pin!(restore);
        tokio::select! {
            result = &mut restore => panic!("aggregate should still be blocked: {}", result.is_ok()),
            () = rpc.wait_for(|requests| requests.iter().any(|request| request["method"] == "eth_call")) => {},
        }
        // Dropping the future is the same cancellation boundary as the panel's Stop.
    }
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
    assert!(retained.iter().all(|record| record.address().is_some()
        && record.restored_at().is_some()
        && record.use_check().observation().is_none()
        && !record.use_check().is_unavailable()));
    drop(owner);
    drop(records);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn restore_batches_64_use_checks_and_preserves_success_after_failure_and_restart() {
    let rpc = Rpc::start().await;
    let results = (0..64)
        .map(|index| IMulticall3::Result {
            success: index != 3,
            returnData: match index {
                0 => U256::ONE.to_be_bytes::<32>().to_vec().into(),
                1 => U256::ZERO.to_be_bytes::<32>().to_vec().into(),
                4 => Bytes::from_static(&[1]),
                _ => Bytes::new(),
            },
        })
        .collect::<Vec<_>>();
    *rpc.state.aggregate_response.lock().unwrap() = Some(
        json!({"result": Bytes::from(IMulticall3::tryAggregateCall::abi_encode_returns(&results))}),
    );
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
    let report = owner
        .discover_range(
            &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
            None,
            0..64,
        )
        .await
        .unwrap();
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
    {
        let requests = rpc.state.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|request| request["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["eth_chainId", "eth_getBlockByNumber", "eth_call"],
            "Restore must not query balances, account nonces, code, storage or history per address"
        );
        let request = &requests[2];
        assert_eq!(request["params"][0]["to"], json!(multicall()));
        assert_eq!(
            request["params"][1],
            json!({"blockHash": B256::repeat_byte(10), "requireCanonical": true})
        );
        let input: Bytes = serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
        let aggregate = IMulticall3::tryAggregateCall::abi_decode(&input).unwrap();
        assert!(!aggregate.requireSuccess);
        assert_eq!(aggregate.calls.len(), 64);
        for (call, record) in aggregate.calls.iter().zip(&saved) {
            assert_eq!(Some(call.target), record.address());
            assert_eq!(call.callData, RelayAdapt7702::nonceCall {}.abi_encode());
        }
    }
    *rpc.state.aggregate_response.lock().unwrap() =
        Some(json!({"error": {"code": -32000, "message": "aggregate failed"}}));
    let report = owner
        .discover_range(
            &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
            None,
            0..64,
        )
        .await
        .unwrap();
    assert_eq!(
        (report.used(), report.unused(), report.unavailable()),
        (0, 0, 64)
    );
    assert_eq!(
        rpc.state.requests.lock().unwrap().len(),
        6,
        "failed aggregate must not fan out"
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
        assert!(after.assets().is_empty());
        assert_eq!(before.operation(), after.operation());
    }
    restarted.shutdown().await;
    drop(restarted);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
