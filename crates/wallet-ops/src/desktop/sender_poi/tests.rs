use super::*;
use alloy::primitives::Bloom;
use broadcaster_core::contracts::railgun::CommitmentCiphertext;
use broadcaster_core::transact::{PreTxPoi, SnarkJsProof};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

fn chain() -> ChainKey {
    ChainKey {
        chain_id: 1,
        contract: Address::repeat_byte(0x11),
    }
}

fn tx_hash() -> FixedBytes<32> {
    FixedBytes::repeat_byte(0x22)
}

fn context(byte: u8) -> SingleCommitmentProofContext {
    SingleCommitmentProofContext {
        txid_version: DEFAULT_TXID_VERSION.to_owned(),
        railgun_txid: U256::from(7),
        utxo_tree_in: 4,
        commitment: FixedBytes::repeat_byte(byte),
        npk: FixedBytes::repeat_byte(0x33),
        pre_transaction_pois_per_txid_leaf_per_list: BTreeMap::from([(
            FixedBytes::repeat_byte(0x44),
            BTreeMap::from([(
                FixedBytes::repeat_byte(0x55),
                PreTxPoi {
                    snark_proof: SnarkJsProof {
                        pi_a: [U256::from(1); 2],
                        pi_b: [[U256::from(2); 2]; 2],
                        pi_c: [U256::from(3); 2],
                    },
                    txid_merkleroot: FixedBytes::repeat_byte(0x66),
                    poi_merkleroots: vec![FixedBytes::repeat_byte(0x77)],
                    blinded_commitments_out: vec![FixedBytes::repeat_byte(0x88)],
                    railgun_txid_if_has_unshield: Bytes::from(vec![0]),
                },
            )]),
        )]),
    }
}

fn receipt_value(commitments: &[FixedBytes<32>]) -> Value {
    let event = Transact {
        treeNumber: U256::from(4),
        startPosition: U256::from(100),
        hash: commitments.to_vec(),
        ciphertext: commitments
            .iter()
            .map(|_| CommitmentCiphertext {
                ciphertext: [FixedBytes::ZERO; 4],
                blindedSenderViewingKey: FixedBytes::ZERO,
                blindedReceiverViewingKey: FixedBytes::ZERO,
                annotationData: Bytes::new(),
                memo: Bytes::new(),
            })
            .collect(),
    }
    .encode_log_data();
    json!({
        "transactionHash": tx_hash(), "transactionIndex": "0x0",
        "blockHash": FixedBytes::<32>::repeat_byte(0x99), "blockNumber": "0x1234",
        "from": Address::ZERO, "to": chain().contract,
        "cumulativeGasUsed": "0x100", "gasUsed": "0x100", "effectiveGasPrice": "0x1",
        "contractAddress": null, "status": "0x1", "type": "0x2", "logsBloom": Bloom::ZERO,
        "logs": [{
            "address": chain().contract, "topics": event.topics(), "data": event.data,
            "transactionHash": tx_hash(), "transactionIndex": "0x0", "logIndex": "0x0",
            "blockHash": FixedBytes::<32>::repeat_byte(0x99), "blockNumber": "0x1234", "removed": false,
        }],
    })
}

#[test]
fn binds_only_prepared_outputs_to_receipt_positions() {
    let a = context(1);
    let b = context(2);
    let unrelated = context(3);
    let receipt = serde_json::from_value(receipt_value(&[
        a.commitment,
        FixedBytes::repeat_byte(9),
        b.commitment,
    ]))
    .unwrap();
    let contexts = BTreeMap::from([
        (a.commitment, a.clone()),
        (b.commitment, b.clone()),
        (unrelated.commitment, unrelated),
    ]);
    assert_eq!(
        observed_outputs(chain().contract, &receipt, &contexts).unwrap(),
        vec![(a.commitment, 4, 100), (b.commitment, 4, 102)]
    );
}

#[test]
fn rejects_removed_mismatched_and_duplicate_receipt_events() {
    let a = context(1);
    let contexts = BTreeMap::from([(a.commitment, a.clone())]);
    for (field, value) in [
        ("removed", json!(true)),
        ("transactionHash", json!(FixedBytes::<32>::ZERO)),
        ("blockHash", json!(FixedBytes::<32>::ZERO)),
        ("blockNumber", json!("0x1235")),
    ] {
        let mut value_receipt = receipt_value(&[a.commitment]);
        value_receipt["logs"][0][field] = value;
        let receipt = serde_json::from_value(value_receipt).unwrap();
        assert!(
            observed_outputs(chain().contract, &receipt, &contexts).is_err(),
            "{field}"
        );
    }
    let receipt = serde_json::from_value(receipt_value(&[a.commitment, a.commitment])).unwrap();
    assert!(observed_outputs(chain().contract, &receipt, &contexts).is_err());
    let receipt = serde_json::from_value(receipt_value(&[a.commitment])).unwrap();
    assert!(
        observed_outputs(Address::ZERO, &receipt, &contexts)
            .unwrap()
            .is_empty()
    );
}

async fn server(
    receipt: Value,
    fail_first_proof: bool,
) -> (Url, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        let mut proof_count = 0;
        let mut head_count = 0;
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buf = [0; 4096];
            let (body_start, length) = loop {
                let count = socket.read(&mut buf).await.unwrap();
                assert!(count > 0);
                data.extend_from_slice(&buf[..count]);
                if let Some(end) = data.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    break (end + 4, length);
                }
            };
            while data.len() < body_start + length {
                let count = socket.read(&mut buf).await.unwrap();
                assert!(count > 0);
                data.extend_from_slice(&buf[..count]);
            }
            let request: Value =
                serde_json::from_slice(&data[body_start..body_start + length]).unwrap();
            captured.lock().unwrap().push(request.clone());
            let response = match request["method"].as_str().unwrap() {
                "eth_blockNumber" => {
                    head_count += 1;
                    json!({"jsonrpc":"2.0", "id":request["id"], "result": if head_count == 1 { "0x1234" } else { "0x1236" }})
                }
                "eth_getTransactionReceipt" => json!({"jsonrpc":"2.0", "id":request["id"], "result":receipt}),
                "ppoi_submit_single_commitment_proofs" => {
                    proof_count += 1;
                    if fail_first_proof && proof_count == 1 {
                        json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":-32000,"message":"not indexed yet"}})
                    } else {
                        json!({"jsonrpc":"2.0", "id":request["id"], "result":true})
                    }
                }
                method => panic!("unexpected RPC method: {method}"),
            }.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
        }
    });
    (url, requests, task)
}

#[tokio::test]
async fn submits_all_receipt_outputs_and_retries_indexing_failure() {
    let a = context(1);
    let b = context(2);
    let (url, requests, server) = server(receipt_value(&[a.commitment, b.commitment]), true).await;
    let rpc = QueryRpcPool::with_http_client(
        vec![url.clone()],
        Duration::from_millis(1),
        reqwest::Client::new(),
    );
    let poi = PoiRpcClient::with_http_client(url, reqwest::Client::new());
    let contexts = BTreeMap::from([(a.commitment, a), (b.commitment, b)]);
    let count = tokio::time::timeout(
        Duration::from_secs(5),
        submit_after_receipt(
            chain(),
            tx_hash(),
            &rpc,
            &poi,
            &contexts,
            0,
            Duration::from_millis(5),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(count, 2);
    let requests = requests.lock().unwrap();
    let proofs: Vec<_> = requests
        .iter()
        .filter(|r| r["method"] == "ppoi_submit_single_commitment_proofs")
        .collect();
    assert_eq!(proofs.len(), 4);
    assert_eq!(
        proofs[0]["params"]["singleCommitmentProofsData"]["utxoPositionOut"],
        100
    );
    assert_eq!(
        proofs[1]["params"]["singleCommitmentProofsData"]["utxoPositionOut"],
        101
    );
    assert_eq!(proofs[0]["params"]["chainID"], "1");
    assert!(proofs[0]["params"].get("networkName").is_none());
    server.abort();
}

#[tokio::test]
async fn reverted_transaction_does_not_submit_any_proofs() {
    let a = context(1);
    let mut receipt = receipt_value(&[a.commitment]);
    receipt["status"] = json!("0x0");
    let (url, requests, server) = server(receipt, false).await;
    let rpc = QueryRpcPool::new(vec![url.clone()], Duration::from_millis(1));
    let poi = PoiRpcClient::with_http_client(url, reqwest::Client::new());
    assert_eq!(
        submit_after_receipt(
            chain(),
            tx_hash(),
            &rpc,
            &poi,
            &BTreeMap::from([(a.commitment, a)]),
            0,
            Duration::from_millis(1)
        )
        .await
        .unwrap(),
        0
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn waits_for_configured_confirmation_depth_before_submitting() {
    let a = context(1);
    let (url, requests, server) = server(receipt_value(&[a.commitment]), false).await;
    let rpc = QueryRpcPool::new(vec![url.clone()], Duration::ZERO);
    let poi = PoiRpcClient::with_http_client(url, reqwest::Client::new());
    let contexts = BTreeMap::from([(a.commitment, a)]);
    let count = tokio::time::timeout(
        Duration::from_secs(5),
        submit_after_receipt(
            chain(),
            tx_hash(),
            &rpc,
            &poi,
            &contexts,
            2,
            Duration::from_millis(5),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(count, 1);
    let requests = requests.lock().unwrap();
    let methods: Vec<_> = requests
        .iter()
        .map(|r| r["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        [
            "eth_getTransactionReceipt",
            "eth_blockNumber",
            "eth_getTransactionReceipt",
            "eth_blockNumber",
            "ppoi_submit_single_commitment_proofs"
        ]
    );
    server.abort();
}

#[tokio::test]
async fn dropping_sending_wallet_keeps_prepared_proofs_running() {
    let context = context(1);
    let (url, requests, server) = server(receipt_value(&[context.commitment]), false).await;
    let outbox = Arc::new(SenderPoiOutbox::default());
    let session = SenderPoiSession::new(
        Arc::clone(&outbox),
        chain(),
        0,
        Arc::new(QueryRpcPool::with_http_client(
            vec![url.clone()],
            Duration::ZERO,
            reqwest::Client::new(),
        )),
        PoiRpcClient::with_http_client(url, reqwest::Client::new()),
    );
    session.retain(&[PendingOutputPoiContextRecord {
        chain_id: 1,
        wallet_id: "test-sender".into(),
        txid_version: context.txid_version,
        output_commitment: context.commitment,
        output_npk: context.npk,
        utxo_tree_in: context.utxo_tree_in,
        railgun_txid: context.railgun_txid,
        txid_merkleroot_index: None,
        pre_transaction_pois_per_txid_leaf_per_list: context
            .pre_transaction_pois_per_txid_leaf_per_list,
        required_poi_list_keys: vec![FixedBytes::repeat_byte(0x44)],
        output_role: PendingOutputPoiRole::Recipient,
        created_at: 1,
        source_operation_id: None,
        observation: None,
        submitted_poi_list_keys: vec![],
        terminal_error: None,
    }]);
    session.submit(tx_hash());
    drop(session);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if outbox.state.lock().unwrap().jobs[&(1, tx_hash())].is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    {
        let requests = requests.lock().unwrap();
        let proofs: Vec<_> = requests
            .iter()
            .filter(|r| r["method"] == "ppoi_submit_single_commitment_proofs")
            .collect();
        assert_eq!(proofs.len(), 1);
        let data = &proofs[0]["params"]["singleCommitmentProofsData"];
        assert_eq!(data["commitment"], hex::encode_prefixed(context.commitment));
        assert_eq!(data["npk"], hex::encode_prefixed(context.npk));
        assert_eq!(data["utxoTreeOut"], 4);
        assert_eq!(data["utxoPositionOut"], 100);
    }
    outbox.cancel(true).await;
    server.abort();
}

#[tokio::test]
async fn proof_job_survives_sender_drop_but_lock_cancels_and_rejects_stale_work() {
    struct OnDrop(Option<oneshot::Sender<()>>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }
    let outbox = Arc::new(SenderPoiOutbox::default());
    let (started, started_rx) = oneshot::channel();
    let (dropped, dropped_rx) = oneshot::channel();
    let session = SenderPoiSession::new(
        Arc::clone(&outbox),
        chain(),
        0,
        Arc::new(QueryRpcPool::new(vec![], Duration::ZERO)),
        PoiRpcClient::with_http_client(
            Url::parse("http://127.0.0.1:1").unwrap(),
            reqwest::Client::new(),
        ),
    );
    session
        .outbox
        .spawn(session.generation, (1, tx_hash()), async move {
            let _on_drop = OnDrop(Some(dropped));
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
    started_rx.await.unwrap();
    drop(session);
    assert!(!outbox.state.lock().unwrap().jobs[&(1, tx_hash())].is_finished());
    outbox.cancel(true).await;
    dropped_rx.await.unwrap();
    outbox.spawn(0, (1, FixedBytes::ZERO), async {
        panic!("stale session must not start work")
    });
    assert!(outbox.state.lock().unwrap().jobs.is_empty());
}

#[tokio::test]
async fn cache_reset_retires_old_generation_without_closing_new_sessions() {
    let outbox = SenderPoiOutbox::default();
    outbox.cancel(false).await;
    outbox.spawn(0, (1, tx_hash()), async { panic!("stale generation") });
    assert!(outbox.state.lock().unwrap().jobs.is_empty());
    let (done, done_rx) = oneshot::channel();
    outbox.spawn(1, (1, tx_hash()), async move {
        let _ = done.send(());
    });
    done_rx.await.unwrap();
    outbox.spawn(1, (1, tx_hash()), async { panic!("duplicate submission") });
    assert_eq!(outbox.state.lock().unwrap().jobs.len(), 1);
    outbox.cancel(true).await;
}
