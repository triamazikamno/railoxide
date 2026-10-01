//! Swap observations from canonical chain data served by a mock RPC, including
//! reorgs that remove the evidence an observation or a reservation release rests on.

use super::*;
use crate::{SwapOrderState, swap_order_state};
use alloy::consensus::transaction::Recovered;
use alloy::consensus::{
    Eip658Value, Receipt, ReceiptEnvelope, SignableTransaction, TxEip1559, TxEnvelope,
};
use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::aliases::U120;
use alloy::primitives::{LogData, Signature, TxKind, address, keccak256};
use alloy::rpc::types::{Log, TransactionReceipt};
use alloy::sol_types::SolEvent;
use broadcaster_core::contracts::across::{
    SpokePool, V3RelayExecutionEventInfo, address_to_bytes32,
};
use broadcaster_core::contracts::cow::{BUY_NATIVE_TOKEN, GPv2Settlement, OrderUid};
use broadcaster_core::contracts::railgun::{
    Call, CommitmentCiphertext, Nullified, Shield, ShieldCiphertext, ShieldRequest, TokenData,
    Transact, shieldCall,
};

const SELL: Address = super::swap_setup::WETH;
const BUY: Address = super::swap_setup::USDC;
const OTHER_BUY: Address = address!("6b175474e89094c44da98b954eedeac495271d0f");
const EXECUTOR: Address = address!("e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0");
const SELL_AMOUNT: u64 = 10_000;
const BUY_AMOUNT: u64 = 9_999;
const PRE_HOOK_NULLIFIER: u8 = 0x21;
const PRE_HOOK_COMMITMENT: u8 = 0x22;

alloy::sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Trade(address indexed owner, address sellToken, address buyToken, uint256 sellAmount, uint256 buyAmount, uint256 feeAmount, bytes orderUid);
    function filledAmount(bytes orderUid) external view returns (uint256);
}

const fn timestamp(number: u64) -> u64 {
    1_000_000 + 12 * number
}

fn value_at(values: &[(u64, u64)], number: u64) -> U256 {
    U256::from(
        values
            .iter()
            .rev()
            .find(|(from, _)| *from <= number)
            .map_or(0, |(_, value)| *value),
    )
}

fn quantity(value: &Value) -> u64 {
    u64::from_str_radix(value.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
}

/// A chain whose block hashes change from each reorg height on. Logs and
/// transactions keep the hash of the block they were added to, so a reorg
/// removes them.
struct MockChain {
    chain_id: u64,
    railgun: Address,
    delegate: Address,
    head: u64,
    reorgs: Vec<u64>,
    nonces: Vec<(u64, u64)>,
    buy_balance: Vec<(u64, u64)>,
    /// Per order, the block from which the settlement reads its fill as zero, as
    /// after it frees an expired order's storage.
    fill_cleared: Vec<(OrderUid, u64)>,
    /// Per order, the block from which the settlement reads its fill as the maximum,
    /// as after `invalidateOrder`.
    invalidated: Vec<(OrderUid, u64)>,
    logs: Vec<Log>,
    transactions: Vec<(B256, alloy::rpc::types::Transaction, TransactionReceipt)>,
    log_queries: usize,
    rpc_methods: Vec<String>,
    rpc_requests: Vec<Value>,
    receipt_error: Option<i64>,
    reorg_on_receipts: bool,
}

impl MockChain {
    const fn new(chain_id: u64, railgun: Address, delegate: Address) -> Self {
        Self {
            chain_id,
            railgun,
            delegate,
            head: 0,
            reorgs: Vec::new(),
            nonces: Vec::new(),
            buy_balance: Vec::new(),
            fill_cleared: Vec::new(),
            invalidated: Vec::new(),
            logs: Vec::new(),
            transactions: Vec::new(),
            log_queries: 0,
            rpc_methods: Vec::new(),
            rpc_requests: Vec::new(),
            receipt_error: None,
            reorg_on_receipts: false,
        }
    }

    fn hash(&self, number: u64) -> B256 {
        let fork = self
            .reorgs
            .iter()
            .rposition(|from| *from <= number)
            .map_or(0, |index| index + 1);
        let mut hash = [0x11; 32];
        hash[..8].copy_from_slice(&number.to_be_bytes());
        hash[8] = u8::try_from(fork).unwrap();
        B256::from(hash)
    }

    fn block(&self, number: u64) -> BlockNumHash {
        BlockNumHash::new(number, self.hash(number))
    }

    fn reorg(&mut self, from: u64) {
        self.reorgs.push(from);
    }

    fn number_of(&self, id: &Value) -> Option<u64> {
        let hash: B256 = serde_json::from_value(id.get("blockHash").unwrap_or(id).clone()).ok()?;
        (0..=self.head).find(|number| self.hash(*number) == hash)
    }

    fn canonical(&self, number: Option<u64>, hash: Option<B256>) -> bool {
        number.is_some_and(|number| number <= self.head && hash == Some(self.hash(number)))
    }

    fn add_logs(&mut self, number: u64, transaction: B256, logs: Vec<(Address, LogData)>) {
        let block_hash = self.hash(number);
        for (address, data) in logs {
            let log_index = Some(self.logs.len() as u64);
            self.logs.push(Log {
                inner: alloy::primitives::Log { address, data },
                block_hash: Some(block_hash),
                block_number: Some(number),
                block_timestamp: None,
                transaction_hash: Some(transaction),
                transaction_index: Some(0),
                log_index,
                removed: false,
            });
        }
    }

    /// A successful transaction sent to `to`, whose receipt carries Railgun `logs`.
    fn add_transaction(&mut self, number: u64, to: Address, input: Bytes, logs: Vec<LogData>) {
        self.add_addressed_transaction(
            number,
            to,
            input,
            logs.into_iter().map(|data| (self.railgun, data)).collect(),
        );
    }

    fn add_addressed_transaction(
        &mut self,
        number: u64,
        to: Address,
        input: Bytes,
        logs: Vec<(Address, LogData)>,
    ) {
        let signed = TxEip1559 {
            chain_id: 1,
            nonce: self.transactions.len() as u64,
            gas_limit: 1_000_000,
            max_fee_per_gas: 1,
            to: TxKind::Call(to),
            input,
            ..TxEip1559::default()
        }
        .into_signed(Signature::test_signature());
        let hash = *signed.hash();
        let block_hash = self.hash(number);
        let from = Address::repeat_byte(0xbb);
        self.add_logs(number, hash, logs);
        let receipt_logs = self
            .logs
            .iter()
            .filter(|log| log.transaction_hash == Some(hash))
            .cloned()
            .collect();
        let receipt = TransactionReceipt {
            inner: ReceiptEnvelope::Eip1559(
                Receipt {
                    status: Eip658Value::Eip658(true),
                    cumulative_gas_used: 1,
                    logs: receipt_logs,
                }
                .with_bloom(),
            ),
            transaction_hash: hash,
            transaction_index: Some(0),
            block_hash: Some(block_hash),
            block_number: Some(number),
            gas_used: 1,
            effective_gas_price: 1,
            blob_gas_used: None,
            blob_gas_price: None,
            from,
            to: Some(to),
            contract_address: None,
        };
        let transaction = alloy::rpc::types::Transaction {
            inner: Recovered::new_unchecked(TxEnvelope::Eip1559(signed), from),
            block_hash: Some(block_hash),
            block_number: Some(number),
            transaction_index: Some(0),
            effective_gas_price: Some(1),
            block_timestamp: None,
        };
        self.transactions.push((hash, transaction, receipt));
    }

    fn transactions_at(
        &self,
        number: u64,
    ) -> impl Iterator<Item = &(B256, alloy::rpc::types::Transaction, TransactionReceipt)> {
        self.transactions.iter().filter(move |(_, transaction, _)| {
            transaction.block_number == Some(number)
                && self.canonical(transaction.block_number, transaction.block_hash)
        })
    }

    /// The settlement's fill of `uid` at `number`, from canonical trades up to it.
    fn filled(&self, uid: &[u8], number: u64) -> U256 {
        if self
            .invalidated
            .iter()
            .any(|(invalidated, from)| invalidated.0[..] == *uid && *from <= number)
        {
            return U256::MAX;
        }
        if self
            .fill_cleared
            .iter()
            .any(|(cleared, from)| cleared.0[..] == *uid && *from <= number)
        {
            return U256::ZERO;
        }
        self.logs
            .iter()
            .filter(|log| {
                log.block_number.is_some_and(|block| block <= number)
                    && self.canonical(log.block_number, log.block_hash)
            })
            .filter_map(|log| Trade::decode_log_data(&log.inner.data).ok())
            .filter(|trade| trade.orderUid[..] == *uid)
            .map(|trade| trade.sellAmount)
            .sum()
    }

    fn respond(&mut self, request: &Value) -> Value {
        self.rpc_methods
            .push(request["method"].as_str().unwrap().to_owned());
        self.rpc_requests.push(request.clone());
        let params = &request["params"];
        let result = match request["method"].as_str().unwrap() {
            "eth_chainId" => json!(format!("0x{:x}", self.chain_id)),
            "eth_blockNumber" => json!(format!("0x{:x}", self.head)),
            "eth_getBlockByNumber" => {
                let number = quantity(&params[0]);
                if number > self.head {
                    Value::Null
                } else {
                    let mut block = Block::<alloy::rpc::types::Transaction>::default();
                    block.header.hash = self.hash(number);
                    block.header.inner.number = number;
                    block.header.inner.parent_hash = self.hash(number.saturating_sub(1));
                    block.header.inner.timestamp = timestamp(number);
                    let transactions = self.transactions_at(number);
                    block.transactions = if params[1].as_bool() == Some(true) {
                        BlockTransactions::Full(
                            transactions
                                .map(|(_, transaction, _)| transaction.clone())
                                .collect(),
                        )
                    } else {
                        BlockTransactions::Hashes(transactions.map(|(hash, ..)| *hash).collect())
                    };
                    serde_json::to_value(block).unwrap()
                }
            }
            "eth_getBlockReceipts" => {
                let number = self.number_of(&params[0]).unwrap();
                if let Some(code) = self.receipt_error {
                    return json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":code, "message":"receipt method unavailable"}});
                }
                let receipts = serde_json::to_value(
                    self.transactions_at(number)
                        .map(|(_, _, receipt)| receipt)
                        .collect::<Vec<_>>(),
                )
                .unwrap();
                if self.reorg_on_receipts {
                    self.reorg_on_receipts = false;
                    self.reorg(number);
                }
                receipts
            }
            "eth_getTransactionReceipt" => {
                let hash: B256 = serde_json::from_value(params[0].clone()).unwrap();
                self.transactions
                    .iter()
                    .find(|(known, transaction, _)| {
                        *known == hash
                            && self.canonical(transaction.block_number, transaction.block_hash)
                    })
                    .map_or(Value::Null, |(_, _, receipt)| {
                        serde_json::to_value(receipt).unwrap()
                    })
            }
            "eth_getCode" => {
                let code = [
                    EIP7702_DELEGATION_DESIGNATOR.as_slice(),
                    self.delegate.as_slice(),
                ]
                .concat();
                serde_json::to_value(Bytes::from(code)).unwrap()
            }
            "eth_call" => {
                let Some(number) = self.number_of(&params[1]) else {
                    return json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "unknown block"}});
                };
                let call = &params[0];
                let to: Address = serde_json::from_value(call["to"].clone()).unwrap();
                let input: Bytes = serde_json::from_value(
                    call.get("input").unwrap_or_else(|| &call["data"]).clone(),
                )
                .unwrap();
                let word = if to == EXECUTOR && input[..4] == RelayAdapt7702::nonceCall::SELECTOR {
                    value_at(&self.nonces, number)
                } else if to == BUY
                    && input[..4] == crate::public_wallet::PublicErc20::balanceOfCall::SELECTOR
                {
                    value_at(&self.buy_balance, number)
                } else if to == OTHER_BUY
                    && input[..4] == crate::public_wallet::PublicErc20::balanceOfCall::SELECTOR
                {
                    U256::ZERO
                } else if input[..4] == filledAmountCall::SELECTOR {
                    // Only the logged settlements fill orders.
                    self.filled(
                        &filledAmountCall::abi_decode(&input).unwrap().orderUid,
                        number,
                    )
                } else if input[..4] == keccak256("nullifiers(uint256,bytes32)")[..4] {
                    // Nothing spends the swap's notes outside the logged settlements.
                    U256::ZERO
                } else {
                    panic!("unexpected eth_call to {to}");
                };
                serde_json::to_value(B256::from(word)).unwrap()
            }
            "eth_getLogs" => {
                self.log_queries += 1;
                let filter = &params[0];
                let (from, to) = (quantity(&filter["fromBlock"]), quantity(&filter["toBlock"]));
                let addresses: Vec<Address> = match &filter["address"] {
                    Value::Array(_) => serde_json::from_value(filter["address"].clone()).unwrap(),
                    address => vec![serde_json::from_value(address.clone()).unwrap()],
                };
                let logs = self
                    .logs
                    .iter()
                    .filter(|log| {
                        addresses.contains(&log.inner.address)
                            && log
                                .block_number
                                .is_some_and(|number| (from..=to).contains(&number))
                            && self.canonical(log.block_number, log.block_hash)
                    })
                    .collect::<Vec<_>>();
                serde_json::to_value(logs).unwrap()
            }
            method => panic!("unexpected RPC method {method}"),
        };
        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
    }
}

const fn ciphertext() -> CommitmentCiphertext {
    CommitmentCiphertext {
        ciphertext: [B256::ZERO; 4],
        blindedSenderViewingKey: B256::ZERO,
        blindedReceiverViewingKey: B256::ZERO,
        annotationData: Bytes::new(),
        memo: Bytes::new(),
    }
}

fn private_transaction(nullifier: u8, commitment: u8) -> Transaction {
    Transaction {
        proof: SnarkProof::default(),
        merkleRoot: B256::ZERO,
        nullifiers: vec![B256::repeat_byte(nullifier)],
        commitments: vec![B256::repeat_byte(commitment)],
        boundParams: BoundParams::new_transact(0, 0, 1, vec![ciphertext()], EXECUTOR, B256::ZERO),
        unshieldPreimage: CommitmentPreimage::empty(),
    }
}

/// The Railgun events of `private_transaction`.
fn private_logs(nullifier: u8, commitment: u8) -> Vec<LogData> {
    vec![
        Nullified {
            treeNumber: 0,
            nullifier: vec![B256::repeat_byte(nullifier)],
        }
        .encode_log_data(),
        Transact {
            treeNumber: U256::ZERO,
            startPosition: U256::ZERO,
            hash: vec![B256::repeat_byte(commitment)],
            ciphertext: vec![ciphertext()],
        }
        .encode_log_data(),
    ]
}

fn execute(transactions: Vec<Transaction>, calls: Vec<Call>, nonce: u64) -> Bytes {
    RelayAdapt7702::executeCall {
        _transactions: transactions,
        _actionData: RelayAdapt7702ActionData {
            requireSuccess: true,
            minGasLimit: U256::ZERO,
            calls,
        },
        _nonce: U256::from(nonce),
        _signature: Bytes::new(),
    }
    .abi_encode()
    .into()
}

/// Each attempt's post-hook shields the full balance with its own note key.
const fn post_hook_shield(attempt: u8) -> ShieldRequest {
    ShieldRequest {
        preimage: CommitmentPreimage {
            npk: B256::repeat_byte(0x50 + attempt),
            token: TokenData::erc20(BUY),
            value: U120::ZERO,
        },
        ciphertext: ShieldCiphertext {
            encryptedBundle: [B256::repeat_byte(0x58 + attempt); 3],
            shieldKey: B256::repeat_byte(0x5f),
        },
    }
}

/// The Railgun event of a post-hook that shielded `amount` after the fee.
fn post_hook_shield_log(attempt: u8, amount: u64) -> LogData {
    let request = post_hook_shield(attempt);
    Shield {
        treeNumber: U256::ZERO,
        startPosition: U256::ZERO,
        commitments: vec![CommitmentPreimage {
            value: U120::from(amount),
            ..request.preimage
        }],
        shieldCiphertext: vec![request.ciphertext],
        fees: vec![U256::from(25)],
    }
    .encode_log_data()
}

fn uid(attempt: u8, valid_to_block: u64) -> OrderUid {
    OrderUid::new(
        B256::repeat_byte(0x60 + attempt),
        EXECUTOR,
        u32::try_from(timestamp(valid_to_block)).unwrap(),
    )
}

fn trade_log(uid: OrderUid) -> LogData {
    trade_log_for(uid, BUY)
}

/// The settlement's Trade event, which reports the order's `CoW` buy token.
fn trade_log_for(uid: OrderUid, buy_token: Address) -> LogData {
    Trade {
        owner: EXECUTOR,
        sellToken: SELL,
        buyToken: buy_token,
        sellAmount: U256::from(SELL_AMOUNT),
        buyAmount: U256::from(BUY_AMOUNT),
        feeAmount: U256::ZERO,
        orderUid: uid.0.to_vec().into(),
    }
    .encode_log_data()
}

/// A private sell-token note at `position`. The swap's pre-hooks spend the one at 3.
fn sell_note(position: u64) -> Utxo {
    Utxo::new(
        broadcaster_core::notes::Note::new_change(U256::ONE, SELL, U256::from(9), [7; 16]),
        0,
        position,
        UtxoSource {
            tx_hash: B256::repeat_byte(9),
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    )
}

struct Fixture {
    root: std::path::PathBuf,
    db: Arc<DbStore>,
    vault: DesktopVaultStore,
    view: Arc<DesktopViewSession>,
    chain: Arc<Mutex<MockChain>>,
    config: crate::settings::EffectiveChainConfig,
    server: tokio::task::JoinHandle<()>,
    store: ExecutorStore,
    owner: ExecutorOwner,
    operation: ExecutorOperationId,
    delegate: Address,
    railgun: Address,
    settlement: Address,
    input: ExecutorInputIdentity,
    setup: B256,
}

impl Fixture {
    /// A swap executor whose fee-only setup won nonce 0 in block 11, reconciled
    /// at confirmed block 13 with execution nonce 1.
    async fn start() -> Self {
        Self::start_for_purpose("Private swap").await
    }

    async fn start_for_purpose(purpose: &str) -> Self {
        let (root, db, vault) = desktop_store_with_vault();
        let view = Arc::new(import_wallet_with_metadata(
            &vault,
            TEST_WALLET_ID,
            "Wallet",
        ));
        let mut config = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        let delegate = config.accepted_executor_profile().unwrap().delegate();
        let railgun = config.require_railgun().unwrap().deployment.contract;
        let settlement = config.swap_profile().unwrap().settlement();
        let chain = Arc::new(Mutex::new(MockChain {
            head: 14,
            nonces: vec![(11, 1)],
            ..MockChain::new(1, railgun, delegate)
        }));
        let served = chain.clone();
        let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request: Value| served.lock().unwrap().respond(&request)),
            Arc::default(),
            Arc::default(),
        )
        .await;
        config.finality_depth = 1;
        config.rpc_route = crate::RpcChainRoute::new(1, vec![endpoint]);
        let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
        let owner = ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            config.clone(),
            HttpContext::direct_for_tests(),
        )
        .unwrap();
        let operation = ExecutorOperationId::random().unwrap();
        // The swap owner's purpose summary marks the record as a swap executor.
        store
            .reserve(
                operation,
                delegate,
                Some(purpose),
                &[ExecutorAsset::Erc20(SELL), ExecutorAsset::Erc20(BUY)],
            )
            .unwrap();
        store.bind_address(operation, EXECUTOR).unwrap();
        let before_setup =
            ExecutorNonceObservation::new(BlockNumHash::new(10, B256::ZERO), U256::ZERO);
        store.reconcile(operation, before_setup, &[]).unwrap();
        let setup_call = execute(vec![private_transaction(0x11, 0x12)], Vec::new(), 0);
        let setup = B256::repeat_byte(3);
        store
            .record_issued(
                operation,
                IssuedExecutorPayload::new(
                    U256::ZERO,
                    delegate,
                    setup,
                    ExecutorPayloadPurpose::Operation,
                    ExecutorPayloadContext::new(setup_call.clone(), before_setup, Vec::new()),
                ),
            )
            .unwrap();
        chain
            .lock()
            .unwrap()
            .add_transaction(11, EXECUTOR, setup_call, private_logs(0x11, 0x12));
        let reconciled = owner.reconcile_history(operation, 10..14).await.unwrap();
        assert_eq!(
            reconciled.record().payload_status(setup),
            Some(ExecutorPayloadStatus::Executed)
        );
        let input = sell_note(3);
        Self {
            root,
            db,
            vault,
            view,
            chain,
            config,
            server,
            store,
            owner,
            operation,
            delegate,
            railgun,
            settlement,
            input: ExecutorInputIdentity::from_utxo(&input),
            setup,
        }
    }

    fn record(&self) -> ExecutorRecord {
        self.store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == self.operation)
            .unwrap()
    }

    /// Record an attempt signed at the record's reconciled nonce `k`. Retries
    /// reuse the proof, so every pre-hook spends the same note.
    fn record_attempt(&self, attempt: u8, valid_to_block: u64) {
        self.record_pair_attempt(attempt, valid_to_block, BUY);
    }

    fn record_pair_attempt(&self, attempt: u8, valid_to_block: u64, buy: Address) {
        self.record_delivery_attempt(attempt, valid_to_block, buy, SwapDelivery::Reshield, None);
    }

    /// External and NEAR Intents attempts sign no post-hook. An Across post-hook's calls
    /// are its surplus shield alone; the deposit is matched against `bridge`.
    fn record_delivery_attempt(
        &self,
        attempt: u8,
        valid_to_block: u64,
        buy: Address,
        delivery: SwapDelivery,
        bridge: Option<BridgeOrderTerms>,
    ) {
        let post_hook = delivery.has_post_hook();
        let mut shield = post_hook_shield(attempt);
        shield.preimage.token = TokenData::erc20(buy);
        let observed = self.record().nonce_observation().unwrap();
        let nonce = observed.nonce().to::<u64>();
        let inputs = vec![self.input.clone()];
        self.store
            .record_swap_attempt(
                self.operation,
                SwapAttempt {
                    submission: None,
                    terms: SwapTerms::new(
                        SELL,
                        buy,
                        SwapRecipient::new(U256::from(7), [8; 32]),
                        self.setup,
                    ),
                    proof: SwapProof::new(B256::repeat_byte(0x20), inputs.clone()),
                    uid: uid(attempt, valid_to_block),
                    delivery,
                    bounds: SwapApprovedBounds {
                        sell_amount: U256::from(SELL_AMOUNT),
                        unshield_amount: None,
                        unshield_fee_bps: U256::ZERO,
                        buy_amount: U256::from(BUY_AMOUNT),
                        private_minimum: U256::from(9_975),
                        shield_fee_bps: U256::from(25),
                        slippage_bps: 50,
                        pre_hook_gas_limit: 1,
                        post_hook_gas_limit: post_hook.then_some(1),
                        hook_cost: Some(U256::ZERO),
                        anchors: Vec::new(),
                        destination_minimum: match &bridge {
                            Some(BridgeOrderTerms::Across(terms)) => Some(terms.output_amount),
                            Some(BridgeOrderTerms::NearIntents(terms)) => {
                                Some(terms.min_amount_out)
                            }
                            None => None,
                        },
                        gas_share_bps: None,
                        gas_estimate: None,
                        gas_allowance: None,
                        gas_price_wei: None,
                        valid_for_secs: None,
                    },
                    invalidates: None,
                    pre_hook: IssuedExecutorPayload::new(
                        U256::from(nonce),
                        self.delegate,
                        B256::repeat_byte(0x30 + attempt),
                        ExecutorPayloadPurpose::SwapPreHook,
                        ExecutorPayloadContext::new(
                            execute(
                                vec![private_transaction(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)],
                                Vec::new(),
                                nonce,
                            ),
                            observed,
                            inputs,
                        ),
                    ),
                    post_hook: post_hook.then(|| {
                        IssuedExecutorPayload::new(
                            U256::from(nonce + 1),
                            self.delegate,
                            B256::repeat_byte(0x40 + attempt),
                            ExecutorPayloadPurpose::SwapPostHook,
                            ExecutorPayloadContext::new(
                                RelayAdapt7702::multicallCall {
                                    _requireSuccess: true,
                                    _calls: vec![Call {
                                        to: EXECUTOR,
                                        value: U256::ZERO,
                                        data: shieldCall {
                                            _shieldRequests: vec![shield],
                                        }
                                        .abi_encode()
                                        .into(),
                                    }],
                                    _nonce: U256::from(nonce + 1),
                                    _signature: Bytes::new(),
                                }
                                .abi_encode()
                                .into(),
                                observed,
                                Vec::new(),
                            ),
                        )
                    }),
                    bridge,
                },
            )
            .unwrap();
    }

    /// Observe up to the head's confirmed block, with finality depth 1.
    async fn observe(&self, head: u64, start: u64) -> ExecutorRecord {
        self.chain.lock().unwrap().head = head;
        self.owner
            .observe_swap(self.operation, start..head)
            .await
            .unwrap()
            .record()
            .clone()
    }

    /// Observe `range` at the current head. Unlike [`Self::observe`], the page may
    /// end before the confirmed block.
    async fn observe_page(&self, range: std::ops::Range<u64>) -> ExecutorRecord {
        self.owner
            .observe_swap(self.operation, range)
            .await
            .unwrap()
            .record()
            .clone()
    }

    fn reserved(&self, record: &ExecutorRecord) -> bool {
        record.reserved_inputs().contains(&self.input)
    }

    async fn finish(self) {
        self.server.abort();
        self.owner.shutdown().await;
        drop(self.owner);
        drop(self.store);
        drop(self.view);
        drop(self.vault);
        drop(self.db);
        std::fs::remove_dir_all(self.root).unwrap();
    }
}

fn state(record: &ExecutorRecord, attempt: usize) -> SwapOrderState {
    swap_order_state(&record.swap().unwrap().orders()[attempt])
}

/// The fixture's account as the swap form's account list offers it.
fn listed(
    owner: &ExecutorOwner,
    operation: ExecutorOperationId,
) -> Option<crate::SwapAccountCandidate> {
    owner
        .swap_account_candidates()
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.operation() == operation)
}

/// A new owner over the same wallet, without this session's RPC or preparation state.
fn restarted_owner(fixture: &Fixture) -> ExecutorOwner {
    ExecutorOwner::new(
        1,
        fixture.db.clone(),
        fixture.view.clone(),
        fixture.config.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap()
}

#[tokio::test]
async fn recorded_swap_quote_defers_nonce_and_reorg_checks_until_preparation() {
    let fixture = Fixture::start().await;
    let restarted = restarted_owner(&fixture);
    let requests = fixture.chain.lock().unwrap().rpc_methods.len();
    // Even after restart, quoting the recorded setup needs no chain observation.
    let executor = restarted
        .swap_order_preview(fixture.operation, false)
        .unwrap();
    assert_eq!(fixture.chain.lock().unwrap().rpc_methods.len(), requests);
    assert!(!executor.requires_setup(), "do not pay for setup again");
    assert!(
        executor.delegated().is_none(),
        "a quote cannot authorize signing"
    );
    let chains =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap();
    let profile = chains.get(1).unwrap().swap_profile().unwrap();
    let builder = railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: fixture.railgun,
        relay_adapt_contract: Address::repeat_byte(5),
    };
    let amount = U256::from(1_000_000);
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(U256::ONE, SELL, amount, [7; 16]),
        0,
        4,
        UtxoSource {
            tx_hash: B256::ZERO,
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    );
    let crate::SwapAmountPlan::Fits(plan) = crate::plan_swap_inputs(
        &builder,
        &profile,
        executor,
        std::slice::from_ref(&input),
        &crate::SwapAmountRequest {
            sell_token: SELL,
            buy_token: BUY,
            amount,
            delivery: SwapDelivery::Reshield,
            byte_budget: None,
        },
        profile.app_data_byte_budget(),
        None,
    )
    .unwrap() else {
        panic!("one note fits the quote");
    };
    let mut review = crate::price_swap_review(
        plan,
        serde_json::from_value(json!({
            "quote": {
                "sellToken": SELL, "buyToken": BUY,
                "sellAmount": "996500", "buyAmount": "3000000000",
                "validTo": 1, "feeAmount": "1000", "gasAmount": "0", "gasPrice": "0",
                "sellTokenPrice": "1000000000000", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }))
        .unwrap(),
        crate::SwapPrice::Unverified,
        U256::from(25),
        U256::from(25),
        50,
        crate::cow::GAS_SHARE_BALANCED_BPS,
        std::time::Duration::from_mins(10),
        1,
        U256::ZERO,
        crate::OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
    )
    .unwrap();
    let minimum = review.suggested_private_minimum();
    let approval = review.approval(minimum, true).unwrap();
    let quoted_request = review.plan().proof_request(&profile).unwrap();
    let quoted_inputs = builder
        .preview_mixed_private_action_plan(std::slice::from_ref(&input), &quoted_request)
        .unwrap()
        .selected_inputs;
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 16;
        chain.nonces.push((15, 2));
    }
    assert_eq!(
        fixture
            .owner
            .refresh_swap_executor(&mut review, 15)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        review
            .plan()
            .swap_executor()
            .delegated()
            .unwrap()
            .observed()
            .nonce(),
        U256::from(2)
    );
    assert_eq!(review.approval(minimum, true).unwrap(), approval);
    // Use the same request and rebuild validator as proving: the fresh nonce must not
    // invalidate the reviewed shape or permit replacing its pinned notes.
    let request = review.plan().proof_request(&profile).unwrap();
    let rebuilt = builder
        .preview_mixed_private_action_plan(std::slice::from_ref(&input), &request)
        .expect("the refreshed nonce must support rebuilding the reviewed pre-hook");
    assert_eq!(rebuilt.selected_inputs, quoted_inputs);
    let mut replacement = input.clone();
    replacement.position += 1;
    assert!(matches!(
        builder.preview_mixed_private_action_plan(&[replacement], &request),
        Err(railgun_wallet::tx::BuildError::PinnedInputUnavailable { .. })
    ));
    let mut changed_nonce = request;
    changed_nonce.executor.as_mut().unwrap().execution_nonce += U256::ONE;
    assert!(matches!(
        builder.preview_mixed_private_action_plan(&[input], &changed_nonce),
        Err(railgun_wallet::tx::BuildError::CompositePlanShapeChanged { .. })
    ));
    // Losing the setup to a reorg must stop execution even though the local quote succeeded.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.reorg(11);
        chain.nonces.clear();
    }
    assert!(
        fixture
            .owner
            .refresh_swap_executor(&mut review, 15)
            .await
            .is_err()
    );
    restarted.shutdown().await;
    fixture.finish().await;
}

#[tokio::test]
async fn reusing_an_account_for_another_pair_does_not_credit_the_old_tokens_shield() {
    let fixture = Fixture::start_for_purpose("Private send").await;
    assert!(!crate::is_swap_record(&fixture.record()));
    // The list reads recorded outcomes, so a set-up account is offered before a restarted
    // session reconciles it.
    let listed_after_restart = listed(&restarted_owner(&fixture), fixture.operation).unwrap();
    assert_eq!(listed_after_restart.address(), EXECUTOR);
    let selected = fixture
        .owner
        .reuse_swap_account(fixture.operation, 13)
        .await
        .unwrap();
    assert_eq!(selected.executor(), EXECUTOR);
    assert!(selected.is_reused());
    fixture.record_attempt(0, 28);
    assert!(
        listed(&fixture.owner, fixture.operation).is_none(),
        "an order that can still execute keeps the account out of the list"
    );
    let expired = fixture.observe(31, 20).await;
    assert!(expired.swap().unwrap().admits_attempt());
    fixture.store.set_hidden(fixture.operation, true).unwrap();
    let hidden = listed(&fixture.owner, fixture.operation)
        .expect("a hidden account whose order ended is offered for another swap");
    assert!(hidden.is_hidden());
    assert_eq!(hidden.last_pair(), Some((SELL, BUY)));
    fixture.chain.lock().unwrap().rpc_methods.clear();
    let preview = fixture
        .owner
        .swap_order_preview(fixture.operation, true)
        .unwrap();
    assert!(preview.is_reused());
    assert!(fixture.chain.lock().unwrap().rpc_methods.is_empty());
    let selected = fixture
        .owner
        .reuse_swap_account(fixture.operation, 30)
        .await
        .unwrap();
    assert_eq!(selected.executor(), EXECUTOR);
    assert!(selected.is_reused());
    assert_eq!(
        fixture
            .chain
            .lock()
            .unwrap()
            .rpc_methods
            .iter()
            .filter(|method| method.as_str() == "eth_blockNumber")
            .count(),
        1,
        "checking setup and prior orders must share one history reconciliation"
    );
    fixture.record_pair_attempt(1, 100, OTHER_BUY);
    assert!(
        fixture
            .owner
            .reuse_swap_account(fixture.operation, 30)
            .await
            .is_err()
    );
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        let trade = Trade {
            owner: EXECUTOR,
            sellToken: SELL,
            buyToken: OTHER_BUY,
            sellAmount: U256::from(SELL_AMOUNT),
            buyAmount: U256::from(BUY_AMOUNT),
            feeAmount: U256::ZERO,
            orderUid: uid(1, 100).0.to_vec().into(),
        };
        logs.push((fixture.settlement, trade.encode_log_data()));
        // An older post-hook wins nonce 2 and shields USDC, not this order's DAI.
        logs.push((fixture.railgun, post_hook_shield_log(0, 9_975)));
        chain.add_logs(33, B256::repeat_byte(0x49), logs);
        chain.nonces.push((33, 3));
    }
    let observed = fixture.observe(35, 31).await;
    assert_eq!(state(&observed, 1), SwapOrderState::Traded);
    assert!(
        observed.swap().unwrap().orders()[0]
            .observations()
            .shielded
            .is_some()
    );
    assert!(
        observed.swap().unwrap().orders()[1]
            .observations()
            .delivered
            .is_none()
    );
    assert!(!observed.swap().unwrap().admits_attempt());
    fixture.finish().await;
}

#[tokio::test]
async fn a_stopped_or_approved_setup_is_not_offered_for_another_swap() {
    let fixture = Fixture::start().await;
    assert!(listed(&fixture.owner, fixture.operation).is_some());
    fixture.owner.stop_swap_setup(fixture.operation).unwrap();
    assert!(listed(&fixture.owner, fixture.operation).is_none());
    fixture.finish().await;

    // A setup whose approved order still awaits placement belongs to that swap.
    let fixture = Fixture::start().await;
    fixture
        .owner
        .record_swap_approval(
            fixture.operation,
            SwapApproval {
                bounds: SwapApprovedBounds {
                    sell_amount: U256::from(SELL_AMOUNT),
                    unshield_amount: None,
                    unshield_fee_bps: U256::ZERO,
                    buy_amount: U256::from(BUY_AMOUNT),
                    private_minimum: U256::from(9_975),
                    shield_fee_bps: U256::from(25),
                    slippage_bps: 50,
                    pre_hook_gas_limit: 1,
                    post_hook_gas_limit: Some(1),
                    hook_cost: Some(U256::ZERO),
                    anchors: Vec::new(),
                    destination_minimum: None,
                    gas_share_bps: None,
                    gas_estimate: None,
                    gas_allowance: None,
                    gas_price_wei: None,
                    valid_for_secs: None,
                },
                price_verified: Some(false),
                price_acknowledged: true,
                delivery: SwapDelivery::Reshield,
                tokens: None,
            },
        )
        .unwrap();
    assert!(listed(&fixture.owner, fixture.operation).is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn swap_delivery_tolerates_dust_retains_shield_evidence_and_reopens_on_reorg() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    let order = uid(0, 100);

    let record = fixture.observe(31, 20).await;
    assert_eq!(state(&record, 0), SwapOrderState::Open);
    assert!(fixture.reserved(&record));
    assert!(matches!(
        fixture.owner.input_locks(&[sell_note(3)]).unwrap().as_slice(),
        [lock] if matches!(lock.reason(), crate::ExecutorInputLockReason::OrderOpen { .. })
    ));

    // One settlement runs the pre-hook, trades, and runs the post-hook.
    let settlement_tx = B256::repeat_byte(0x40);
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        logs.push((fixture.settlement, trade_log(order)));
        logs.push((fixture.railgun, post_hook_shield_log(0, 9_975)));
        chain.add_logs(40, settlement_tx, logs);
        chain.nonces.push((40, 3));
        // A large gift makes delivery ambiguous on the first page. Later only dust remains.
        chain.buy_balance.push((41, BUY_AMOUNT));
    }
    let record = fixture.observe(46, 35).await;
    assert_eq!(state(&record, 0), SwapOrderState::NotDelivered);
    fixture.chain.lock().unwrap().buy_balance.push((46, 1));
    // The next page contains no settlement logs. Delivery still uses the saved shield.
    let record = fixture.observe(47, 46).await;
    let settled = fixture.chain.lock().unwrap().block(40);
    let observed = record.swap().unwrap().orders()[0].observations();
    assert_eq!(state(&record, 0), SwapOrderState::Done);
    assert_eq!(
        (observed.pre_hook_executed, observed.traded),
        (
            Some(SwapObservation {
                block: settled,
                transaction_hash: Some(settlement_tx)
            }),
            Some(SwapObservation {
                block: settled,
                transaction_hash: Some(settlement_tx)
            })
        )
    );
    // The executed amounts come from the order's Trade log and stay with the retained trade.
    assert_eq!(
        observed.trade_amounts,
        Some(crate::vault::SwapTradeAmounts {
            sell_amount: U256::from(SELL_AMOUNT),
            buy_amount: U256::from(BUY_AMOUNT),
            fee_amount: U256::ZERO,
            settlement_gas_used: None,
            settlement_effective_gas_price: None,
            executed_fee: None,
            executed_fee_token: None,
        })
    );
    // The retained shield keeps the credited amount with the fee its event charged.
    assert_eq!(
        observed
            .shielded
            .map(|shield| (shield.private_amount, shield.fee)),
        Some((U256::from(9_975), Some(U256::from(25))))
    );
    // Delivery rests on the balance at the confirmed block, after the settlement.
    assert_eq!(
        observed.delivered,
        Some(SwapObservation {
            block: fixture.chain.lock().unwrap().block(46),
            transaction_hash: Some(settlement_tx)
        })
    );
    assert!(
        fixture.reserved(&record),
        "spent inputs stay protected while private sync catches up"
    );
    let available = fixture
        .owner
        .inputs_for_record(vec![sell_note(3), sell_note(4)], &record)
        .unwrap();
    assert_eq!(
        available
            .iter()
            .map(|input| input.position)
            .collect::<Vec<_>>(),
        vec![4],
        "reusing the account cannot select its earlier swap's spent note"
    );

    // A reorg drops the settlement: every observation reopens and the inputs are reserved again.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.reorg(38);
        chain.nonces = vec![(11, 1)];
    }
    let record = fixture.observe(47, 40).await;
    assert_eq!(
        record.swap().unwrap().orders()[0].observations(),
        SwapOrderObservations::default()
    );
    assert_eq!(state(&record, 0), SwapOrderState::Open);
    assert!(fixture.reserved(&record));

    // On the new chain a funded post-hook ran before the payout: its shield is in the
    // settlement, but the proceeds stay in the executor, so the swap is not delivered.
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        logs.push((fixture.railgun, post_hook_shield_log(0, 9_975)));
        logs.push((fixture.settlement, trade_log(order)));
        chain.add_logs(50, B256::repeat_byte(0x50), logs);
        chain.nonces.push((50, 3));
        chain.buy_balance.push((50, BUY_AMOUNT));
    }
    let record = fixture.observe(56, 48).await;
    let observed = record.swap().unwrap().orders()[0].observations();
    assert_eq!(state(&record, 0), SwapOrderState::NotDelivered);
    assert_eq!(
        observed.traded.map(|traded| traded.block),
        Some(fixture.chain.lock().unwrap().block(50))
    );
    assert!(observed.delivered.is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn a_shield_below_the_approved_private_minimum_does_not_complete_the_swap() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        logs.push((fixture.settlement, trade_log(uid(0, 100))));
        // Matching post-hook evidence and an empty executor are insufficient when the
        // credited amount is below the minimum the user approved.
        logs.push((fixture.railgun, post_hook_shield_log(0, 9_974)));
        chain.add_logs(40, B256::repeat_byte(0x40), logs);
        chain.nonces.push((40, 3));
    }
    let record = fixture.observe(46, 35).await;
    assert_ne!(state(&record, 0), SwapOrderState::Done);
    assert!(
        record.swap().unwrap().orders()[0]
            .observations()
            .delivered
            .is_none()
    );
    fixture.finish().await;
}

/// Whether any request read an ERC-20 balance.
fn reads_a_balance(requests: &[Value]) -> bool {
    let selector =
        alloy::hex::encode_prefixed(crate::public_wallet::PublicErc20::balanceOfCall::SELECTOR);
    requests.iter().any(|request| {
        request["method"] == "eth_call"
            && request["params"][0]
                .get("input")
                .or_else(|| request["params"][0].get("data"))
                .and_then(Value::as_str)
                .is_some_and(|input| input.starts_with(&selector))
    })
}

#[tokio::test]
async fn reconciling_an_external_swap_delivers_on_its_trade_without_reading_balances() {
    let fixture = Fixture::start().await;
    let receiver = Address::repeat_byte(0x77);
    fixture.record_delivery_attempt(0, 100, BUY, SwapDelivery::External { receiver }, None);
    // One settlement runs the pre-hook and pays the receiver. Only the pre-hook takes a nonce.
    let settlement_tx = B256::repeat_byte(0x40);
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        logs.push((fixture.settlement, trade_log(uid(0, 100))));
        chain.add_logs(40, settlement_tx, logs);
        chain.nonces.push((40, 2));
        chain.rpc_requests.clear();
    }
    let record = fixture.observe(46, 35).await;
    let settled = Some(SwapObservation {
        block: fixture.chain.lock().unwrap().block(40),
        transaction_hash: Some(settlement_tx),
    });
    let observed = record.swap().unwrap().orders()[0].observations();
    assert_eq!(state(&record, 0), SwapOrderState::Done);
    assert_eq!(
        (observed.traded, observed.delivered, observed.undelivered),
        (settled, settled, None)
    );
    assert!(record.swap().unwrap().admits_attempt());
    assert!(!reads_a_balance(
        &fixture.chain.lock().unwrap().rpc_requests
    ));
    fixture.finish().await;
}

#[tokio::test]
async fn an_external_pre_hook_without_a_trade_recovers_only_the_sell_token() {
    let fixture = Fixture::start().await;
    let receiver = Address::repeat_byte(0x77);
    fixture.record_delivery_attempt(
        0,
        35,
        Address::ZERO,
        SwapDelivery::External { receiver },
        None,
    );
    {
        let mut chain = fixture.chain.lock().unwrap();
        let logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect();
        chain.add_logs(30, B256::repeat_byte(0x30), logs);
        chain.nonces.push((30, 2));
    }
    let record = fixture.observe(41, 25).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::PreHookOnly { expired: true }
    );
    // The expired order can't fill, so recovery only resets the sell token's approval.
    let calls = crate::swap_recovery_calls(
        &record,
        &swap_profile(),
        EXECUTOR,
        &[(SELL, U256::from(SELL_AMOUNT))],
        at_block(41),
    )
    .unwrap();
    let [reset] = calls.as_slice() else {
        panic!("recovery resets only the sell token's approval");
    };
    let approval =
        broadcaster_core::contracts::railgun::approveCall::abi_decode(&reset.data).unwrap();
    assert_eq!(
        (reset.to, approval.spender, approval.amount),
        (SELL, swap_profile().vault_relayer(), U256::ZERO)
    );
    fixture.finish().await;
}

#[tokio::test]
async fn an_unchanged_open_order_reads_no_logs_until_its_fill_moves() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    let log_queries = || fixture.chain.lock().unwrap().log_queries;

    // The pre-hook's nonce is unused, nothing is filled, and `validTo` is ahead.
    let record = fixture.observe(31, 20).await;
    assert_eq!(state(&record, 0), SwapOrderState::Open);
    assert_eq!(log_queries(), 0);

    // A solver skips the pre-hook and settles from funds the executor already holds:
    // the nonce stays unused and only the fill shows the trade.
    fixture.chain.lock().unwrap().add_logs(
        33,
        B256::repeat_byte(0x33),
        vec![(fixture.settlement, trade_log(uid(0, 100)))],
    );
    let record = fixture.observe(36, 31).await;
    assert_eq!(state(&record, 0), SwapOrderState::Traded);
    assert!(log_queries() > 0);
    fixture.finish().await;

    // Past `validTo` the settlement may clear an order's fill even though it traded, so
    // a zero fill no longer rules out a trade in pages up to `validTo`.
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 28);
    let log_queries = || fixture.chain.lock().unwrap().log_queries;
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.add_logs(
            25,
            B256::repeat_byte(0x25),
            vec![(fixture.settlement, trade_log(uid(0, 28)))],
        );
        chain.fill_cleared.push((uid(0, 28), 30));
        chain.head = 41;
    }
    // A historical page before the trade records the expiry.
    let record = fixture.observe_page(20..25).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    // A page wholly after `validTo` can't hold a trade, and its logs aren't read.
    let before = log_queries();
    fixture.observe_page(40..41).await;
    assert_eq!(log_queries(), before);
    // The trade's page is still read and finds it.
    let record = fixture.observe_page(25..30).await;
    assert_eq!(state(&record, 0), SwapOrderState::Traded);
    fixture.finish().await;
}

/// Reconcile at the head's confirmed block without recording swap observations, then
/// read whether Public signing may treat every order as closed there.
async fn orders_closed_at(fixture: &Fixture, head: u64) -> (ExecutorRecord, eyre::Result<bool>) {
    fixture.chain.lock().unwrap().head = head;
    let record = fixture
        .owner
        .reconcile_history(fixture.operation, head - 1..head)
        .await
        .unwrap()
        .record()
        .clone();
    let closed = fixture
        .owner
        .swap_orders_closed(&fixture.config, &record)
        .await;
    (record, closed)
}

#[tokio::test]
async fn public_signing_needs_every_order_closed_at_the_fresh_confirmed_block() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 40);
    let order = uid(0, 40);

    // The nonce passes both hooks, which resolves them, but the signed order can still fill.
    fixture.chain.lock().unwrap().nonces.push((20, 3));
    let (record, closed) = orders_closed_at(&fixture, 22).await;
    assert!(!record.has_unresolved_issued_work());
    assert!(!closed.unwrap());

    // A full fill closes it, read from the settlement without a saved trade observation.
    fixture.chain.lock().unwrap().add_logs(
        23,
        B256::repeat_byte(0x23),
        vec![(fixture.settlement, trade_log(order))],
    );
    let (filled, closed) = orders_closed_at(&fixture, 25).await;
    assert!(closed.unwrap());
    assert_eq!(state(&filled, 0), SwapOrderState::Open);

    // A reorg removes the fill while the hook nonces stay spent. The earlier confirmed
    // block no longer counts, and the fresh one shows the order open again.
    fixture.chain.lock().unwrap().reorg(23);
    assert!(
        fixture
            .owner
            .swap_orders_closed(&fixture.config, &filled)
            .await
            .is_err()
    );
    let (_, closed) = orders_closed_at(&fixture, 25).await;
    assert!(!closed.unwrap());

    // Invalidation and expiry each close the order.
    fixture.chain.lock().unwrap().invalidated.push((order, 26));
    let (_, closed) = orders_closed_at(&fixture, 28).await;
    assert!(closed.unwrap());
    fixture.chain.lock().unwrap().invalidated.clear();
    let (expired, closed) = orders_closed_at(&fixture, 42).await;
    assert!(closed.unwrap());

    // The confirmed block can't be read.
    fixture.chain.lock().unwrap().head = 30;
    assert!(
        fixture
            .owner
            .swap_orders_closed(&fixture.config, &expired)
            .await
            .is_err()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn expired_pre_hook_releases_inputs_at_finality_and_a_reorg_restores_them() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 28);

    // `validTo` passed at the confirmed block while the nonce stayed unused.
    let record = fixture.observe(31, 21).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    assert!(!fixture.reserved(&record));

    let requests = fixture.chain.lock().unwrap().rpc_methods.len();
    let restarted = restarted_owner(&fixture);
    let retained = restarted
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == fixture.operation)
        .unwrap();
    assert_eq!(retained.nonce_observation(), record.nonce_observation());
    assert!(
        !fixture.reserved(&retained),
        "finalized release survives restart"
    );
    assert!(restarted.input_locks(&[sell_note(3)]).unwrap().is_empty());
    assert_eq!(fixture.chain.lock().unwrap().rpc_methods.len(), requests);
    restarted.shutdown().await;

    // The chain reorganizes to a shorter one whose confirmed block is before `validTo`.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.reorg(25);
    }
    let record = fixture.observe(27, 20).await;
    assert_eq!(state(&record, 0), SwapOrderState::Open);
    assert!(fixture.reserved(&record));

    let record = fixture.observe(41, 27).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    assert!(!fixture.reserved(&record));
    fixture.finish().await;
}

#[tokio::test]
async fn checking_an_expired_orders_lock_observes_the_order_and_frees_its_notes() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 28);
    let notes = [sell_note(3)];
    // `validTo` passed, but no observation recorded the pre-hook's death yet.
    let locks = fixture.owner.input_locks(&notes).unwrap();
    assert_eq!(locks.len(), 1);
    assert_eq!(locks[0].operation(), fixture.operation);

    fixture.chain.lock().unwrap().head = 31;
    let lock = fixture
        .owner
        .check_input_lock(fixture.operation, 30, &notes)
        .await
        .unwrap();
    assert!(lock.is_none());
    assert_eq!(
        state(&fixture.record(), 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    assert!(fixture.owner.input_locks(&notes).unwrap().is_empty());
    fixture.finish().await;
}

#[tokio::test]
async fn cancellation_and_an_older_post_hook_end_attempts_but_a_copied_shield_does_not() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 200);
    let first = uid(0, 200);

    // A broadcaster-funded cancellation at the pre-hook's nonce invalidates the order.
    let observed = fixture.record().nonce_observation().unwrap();
    let cancellation_call = execute(
        vec![private_transaction(0x31, 0x32)],
        vec![Call {
            to: fixture.settlement,
            value: U256::ZERO,
            data: GPv2Settlement::invalidateOrderCall {
                orderUid: first.0.to_vec().into(),
            }
            .abi_encode()
            .into(),
        }],
        1,
    );
    fixture
        .store
        .record_issued(
            fixture.operation,
            IssuedExecutorPayload::new(
                U256::ONE,
                fixture.delegate,
                B256::repeat_byte(0x39),
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(cancellation_call.clone(), observed, Vec::new()),
            ),
        )
        .unwrap();
    let include_cancellation = |number: u64| {
        let mut chain = fixture.chain.lock().unwrap();
        chain.add_transaction(
            number,
            EXECUTOR,
            cancellation_call.clone(),
            private_logs(0x31, 0x32),
        );
        chain.nonces.push((number, 2));
    };
    include_cancellation(30);
    let record = fixture.observe(36, 25).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)
    );
    assert!(!fixture.reserved(&record));

    // The retry signs its pre-hook at k + 1, the nonce of the first attempt's post-hook.
    fixture.record_attempt(1, 300);
    let record = fixture.record();
    assert!(fixture.reserved(&record));

    // A shield with the older post-hook's public request, without that post-hook taking the
    // nonce, doesn't identify it.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.add_logs(
            58,
            B256::repeat_byte(0x58),
            vec![(fixture.railgun, post_hook_shield_log(0, 9_975))],
        );
    }
    let record = fixture.observe(61, 57).await;
    assert_eq!(state(&record, 1), SwapOrderState::Open);

    // The older post-hook takes the retry's nonce inside a settlement.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.add_logs(
            62,
            B256::repeat_byte(0x62),
            vec![(fixture.railgun, post_hook_shield_log(0, 9_975))],
        );
        chain.nonces.push((62, 3));
    }
    let record = fixture.observe(66, 61).await;
    assert_eq!(
        state(&record, 1),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::OlderPostHook)
    );
    // The retry's order can still fill; the cancelled one can't. The next retry's pre-hook
    // invalidates exactly the retry's order.
    let profile =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .unwrap()
            .swap_profile()
            .unwrap();
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(timestamp(66));
    assert_eq!(
        crate::swap_invalidation(&record, &profile, now).unwrap(),
        Some(uid(1, 300))
    );
    // No direct call to the executor won nonce k + 1, but the observed post-hook resolves it,
    // so the next retry is admitted at k + 2 while ordinary operations stay blocked.
    fixture.record_attempt(2, 400);
    let record = fixture.record();
    assert_eq!(
        record.swap().unwrap().orders()[2].pre_hook().nonce(),
        U256::from(3)
    );
    let observed = record.nonce_observation().unwrap();
    assert!(matches!(
        fixture.store.record_issued(
            fixture.operation,
            IssuedExecutorPayload::new(
                U256::from(3),
                fixture.delegate,
                B256::repeat_byte(0x3f),
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"operation"), observed, Vec::new()),
            ),
        ),
        Err(ExecutorStoreError::OutstandingNonce)
    ));
    fixture.finish().await;
}

fn swap_profile() -> crate::settings::SwapProfile {
    crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
        .unwrap()
        .get(1)
        .unwrap()
        .swap_profile()
        .unwrap()
}

fn at_block(number: u64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + Duration::from_secs(timestamp(number))
}

fn hook_payload(
    record: &ExecutorRecord,
    purpose: ExecutorPayloadPurpose,
) -> &IssuedExecutorPayload {
    record
        .issued()
        .iter()
        .find(|payload| payload.purpose() == purpose)
        .unwrap()
}

/// Checks recovery's leading calls: `invalidateOrder(uid)` on the settlement, then a reset of
/// the sell token's approval to the vault relayer.
fn assert_invalidates_then_resets(calls: &[Call], settlement: Address, uid: OrderUid) {
    assert_eq!(calls.len(), 2);
    let invalidated = GPv2Settlement::invalidateOrderCall::abi_decode(&calls[0].data).unwrap();
    assert_eq!(
        (calls[0].to, &invalidated.orderUid[..]),
        (settlement, &uid.0[..])
    );
    let reset =
        broadcaster_core::contracts::railgun::approveCall::abi_decode(&calls[1].data).unwrap();
    assert_eq!(
        (calls[1].to, reset.spender, reset.amount),
        (SELL, swap_profile().vault_relayer(), U256::ZERO)
    );
}

#[tokio::test]
async fn recovery_invalidates_an_open_order_but_an_expired_pre_hook_is_not_outstanding() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 28);
    let profile = swap_profile();
    let k = U256::ONE;

    // The open order can fill, and its pre-hook holds the current nonce k.
    let record = fixture.record();
    let pre_hook = hook_payload(&record, ExecutorPayloadPurpose::SwapPreHook);
    assert!(record.is_outstanding_at(pre_hook, k));
    assert!(record.has_competing_payloads());
    // The post-hook at k + 1 is the swap exception, not recorded state ahead of the chain.
    assert!(!record.records_future_nonce(k));
    let calls = crate::swap_recovery_calls(
        &record,
        &profile,
        EXECUTOR,
        &[(SELL, U256::from(SELL_AMOUNT))],
        at_block(20),
    )
    .unwrap();
    assert_invalidates_then_resets(&calls, fixture.settlement, uid(0, 28));
    crate::swap_cancellation_admitted(&record, k, &profile, at_block(20)).unwrap();

    // `validTo` passed at finality with k unused: nothing is outstanding at k, so recovery
    // warns of no competing payload and adds no call for the dead pre-hook.
    let record = fixture.observe(31, 21).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    assert!(
        !record
            .issued()
            .iter()
            .any(|payload| record.is_outstanding_at(payload, k))
    );
    assert!(!record.has_competing_payloads());
    assert!(
        crate::swap_recovery_calls(&record, &profile, EXECUTOR, &[], at_block(31))
            .unwrap()
            .is_empty()
    );
    assert!(crate::swap_cancellation_admitted(&record, k, &profile, at_block(31)).is_err());
    fixture.finish().await;
}

#[tokio::test]
async fn a_settled_pre_hook_beats_a_cancellation_and_recovery_is_offered() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 200);
    let observed = fixture.record().nonce_observation().unwrap();
    let cancellation = |hash: u8, inputs: Vec<ExecutorInputIdentity>| {
        IssuedExecutorPayload::new(
            U256::ONE,
            fixture.delegate,
            B256::repeat_byte(hash),
            ExecutorPayloadPurpose::Recovery,
            ExecutorPayloadContext::new(
                execute(vec![private_transaction(0x31, 0x32)], Vec::new(), 1),
                observed,
                inputs,
            ),
        )
    };
    // The cancellation's fee can't spend the note the swap's own pre-hook reserves.
    assert!(matches!(
        fixture.store.record_issued(
            fixture.operation,
            cancellation(0x38, vec![fixture.input.clone()])
        ),
        Err(ExecutorStoreError::InputReserved)
    ));
    let fee = ExecutorInputIdentity::from_utxo(&sell_note(4));
    let cancelled = fixture
        .store
        .record_issued(fixture.operation, cancellation(0x39, vec![fee.clone()]))
        .unwrap();
    assert!(cancelled.reserved_inputs().contains(&fee));

    // A solver runs the pre-hook inside a settlement before the cancellation confirms.
    {
        let mut chain = fixture.chain.lock().unwrap();
        let logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect();
        chain.add_logs(30, B256::repeat_byte(0x30), logs);
        chain.nonces.push((30, 2));
    }
    let record = fixture.observe(36, 25).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::PreHookOnly { expired: false }
    );
    assert_ne!(
        record.payload_status(B256::repeat_byte(0x39)),
        Some(ExecutorPayloadStatus::Executed)
    );
    assert!(
        !record.reserved_inputs().contains(&fee),
        "the loser's fee note is released"
    );

    // Recovery competes only with the post-hook at the current nonce k + 1, and invalidates
    // the order that can still fill with the executor's funds.
    let post_hook = hook_payload(&record, ExecutorPayloadPurpose::SwapPostHook);
    assert!(record.is_outstanding_at(post_hook, U256::from(2)));
    assert!(!record.records_future_nonce(U256::from(2)));
    let calls = crate::swap_recovery_calls(
        &record,
        &swap_profile(),
        EXECUTOR,
        &[(SELL, U256::from(SELL_AMOUNT))],
        at_block(36),
    )
    .unwrap();
    assert_invalidates_then_resets(&calls, fixture.settlement, uid(0, 200));
    fixture.finish().await;
}

#[tokio::test]
async fn cancellation_fee_selection_excludes_the_swaps_reserved_notes() {
    use railgun_wallet::tx::{
        BuildError, MixedPrivateActionRequest, MixedPrivateSend, MixedPrivateSendRole,
        SelectedInputIdentity,
    };

    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 200);
    let record = fixture.record();
    let builder = railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: fixture.railgun,
        relay_adapt_contract: Address::repeat_byte(5),
    };
    // The broadcaster fee is paid in the swap's sell token.
    let fee = MixedPrivateActionRequest {
        executor: None,
        executor_calls: Vec::new(),
        private_sends: vec![MixedPrivateSend {
            token_address: SELL,
            amount: U256::ONE,
            recipient: broadcaster_core::crypto::railgun::AddressData {
                master_public_key: U256::ONE,
                viewing_public_key: [1; 32],
            },
            role: MixedPrivateSendRole::Other,
        }],
        public_unshields: Vec::new(),
        relay_actions: None,
        min_gas_price: 1,
        verify_proof: false,
        spend_up_to: false,
        rebuild: None,
    };
    let reserved = sell_note(3);
    let inputs = fixture
        .owner
        .inputs_for_recovery(vec![reserved.clone()], &record)
        .unwrap();
    assert!(matches!(
        builder.preview_mixed_private_action_plan(&inputs, &fee),
        Err(BuildError::InsufficientBalance(_) | BuildError::InsufficientFeeTokenBalance(_))
    ));
    let spare = sell_note(4);
    let inputs = fixture
        .owner
        .inputs_for_recovery(vec![reserved.clone(), spare.clone()], &record)
        .unwrap();
    assert_eq!(
        builder
            .preview_mixed_private_action_plan(&inputs, &fee)
            .unwrap()
            .selected_inputs,
        vec![SelectedInputIdentity::from_utxo(&spare)]
    );
    // A release frees the notes for other operations, but the swap's own cancellation still
    // competes with its pre-hook for them.
    fixture.owner.release_input_lock(fixture.operation).unwrap();
    let released = fixture.record();
    assert!(!fixture.reserved(&released));
    let inputs = fixture
        .owner
        .inputs_for_recovery(vec![reserved, spare.clone()], &released)
        .unwrap();
    assert_eq!(
        inputs
            .iter()
            .map(|input| input.position)
            .collect::<Vec<_>>(),
        vec![4]
    );
    fixture.finish().await;
}

fn settlement_logs(fixture: &Fixture, attempt: u8, valid_to: u64) -> Vec<(Address, LogData)> {
    vec![
        (fixture.settlement, trade_log(uid(attempt, valid_to))),
        (
            BUY,
            Transfer {
                from: fixture.settlement,
                to: EXECUTOR,
                value: U256::from(BUY_AMOUNT),
            }
            .encode_log_data(),
        ),
        (
            BUY,
            Transfer {
                from: EXECUTOR,
                to: fixture.railgun,
                value: U256::from(BUY_AMOUNT - 24),
            }
            .encode_log_data(),
        ),
        (
            BUY,
            Transfer {
                from: EXECUTOR,
                to: Address::repeat_byte(0xfe), // Railgun treasury receives the fee separately.
                value: U256::from(25),
            }
            .encode_log_data(),
        ),
        (
            fixture.railgun,
            post_hook_shield_log(attempt, BUY_AMOUNT - 24),
        ),
    ]
}

fn assert_block_only_requests(requests: &[Value], block: BlockNumHash) {
    assert!(!requests.is_empty());
    for request in requests {
        match request["method"].as_str().unwrap() {
            "eth_chainId" | "eth_blockNumber" => {
                assert!(request["params"].is_null() || request["params"] == json!([]));
            }
            "eth_getBlockByNumber" => assert_eq!(
                request["params"],
                json!([format!("0x{:x}", block.number), false])
            ),
            "eth_getBlockReceipts" => assert_eq!(
                request["params"],
                json!([alloy::eips::BlockId::from(block.hash)])
            ),
            method => panic!("identifying RPC method in routine confirmation: {method}"),
        }
    }
}

#[tokio::test]
async fn pending_observations_release_activity_and_reject_concurrent_changes() {
    // Exercise both history reconciliation and receipt-only settlement: neither may
    // hold activity over RPC, and neither may overwrite a changed local record.
    for settlement in [false, true] {
        let fixture = Fixture::start().await;
        fixture.record_attempt(1, 20);
        let logs = settlement_logs(&fixture, 1, 20);
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 16;
            chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
        }
        let gate = crate::rpc_broker::tests::RpcMockGate {
            request_started: Arc::default(),
            release_response: Arc::default(),
        };
        let served = fixture.chain.clone();
        let (endpoint, server) = crate::rpc_broker::tests::spawn_gated_rpc_mock(
            Arc::new(move |request| served.lock().unwrap().respond(&request)),
            Arc::default(),
            Arc::default(),
            gate.clone(),
        )
        .await;
        let mut config = fixture.config.clone();
        config.rpc_route = crate::RpcChainRoute::new(1, vec![endpoint]);
        let owner = ExecutorOwner::new(
            1,
            fixture.db.clone(),
            fixture.view.clone(),
            config,
            HttpContext::direct_for_tests(),
        )
        .unwrap();
        let operation = fixture.operation;
        let pending = async {
            if settlement {
                owner
                    .observe_swap_settlement(operation, uid(1, 20), 15)
                    .await
            } else {
                owner.reconcile_history(operation, 15..16).await.map(|_| ())
            }
        };
        tokio::pin!(pending);
        tokio::select! {
            () = gate.request_started.notified() => {},
            result = &mut pending => panic!("observation did not wait: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(5)) => panic!("observation never started"),
        }
        assert!(
            tokio::time::timeout(
                Duration::from_secs(2),
                owner.reconcile_history(ExecutorOperationId::random().unwrap(), 1..2),
            )
            .await
            .expect("pending observation must release activity")
            .is_err()
        );
        owner.set_hidden(fixture.operation, true).unwrap();
        let changed = fixture.record();
        gate.release_response.notify_one();
        let release = tokio::spawn(async move {
            loop {
                gate.request_started.notified().await;
                gate.release_response.notify_one();
            }
        });
        let error = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            error.to_string().contains("changed during preparation"),
            "{error:#}"
        );
        assert_eq!(
            fixture.record(),
            changed,
            "late observation changed the record"
        );
        release.abort();
        server.abort();
        owner.shutdown().await;
        fixture.finish().await;
    }
}

#[tokio::test]
async fn settlement_receipts_confirm_at_safety_depth_without_account_queries() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(1, 20);
    let logs = settlement_logs(&fixture, 1, 20);
    let nonce_before = fixture.record().nonce_observation();
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 15;
        // Another transaction in the block, whose gas isn't the settlement's.
        chain.add_addressed_transaction(15, Address::repeat_byte(0x55), Bytes::new(), Vec::new());
        let other = &mut chain.transactions.last_mut().unwrap().2;
        (other.gas_used, other.effective_gas_price) = (21_000, 9);
        chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
        let receipt = &mut chain.transactions.last_mut().unwrap().2;
        (receipt.gas_used, receipt.effective_gas_price) = (187_654, 2_345_678_901);
        chain.rpc_requests.clear();
    }
    // CoW can locate an unconfirmed block but cannot advance canonical progress.
    fixture
        .owner
        .observe_swap_settlement(fixture.operation, uid(1, 20), 15)
        .await
        .unwrap();
    assert!(
        fixture.record().swap().unwrap().orders()[0]
            .observations()
            .traded
            .is_none()
    );
    fixture.chain.lock().unwrap().head = 16;
    fixture
        .owner
        .observe_swap_settlement(fixture.operation, uid(1, 20), 15)
        .await
        .unwrap();
    let record = fixture.record();
    let order = &record.swap().unwrap().orders()[0];
    assert_eq!(swap_order_state(order), SwapOrderState::Done);
    assert_eq!(
        order
            .observations()
            .settlement_credit
            .unwrap()
            .private_amount,
        U256::from(BUY_AMOUNT - 24)
    );
    assert!(
        order.observations().shielded.is_none(),
        "receipt credit does not invent hook nonce evidence"
    );
    // The settlement's gas comes from the receipt that emits the trade, without another read.
    let amounts = order.observations().trade_amounts.unwrap();
    assert_eq!(
        (
            amounts.settlement_gas_used,
            amounts.settlement_effective_gas_price
        ),
        (Some(187_654), Some(2_345_678_901))
    );
    assert_eq!(record.nonce_observation(), nonce_before);
    assert!(
        fixture.reserved(&record),
        "spent inputs stay reserved until private sync catches up"
    );
    {
        let chain = fixture.chain.lock().unwrap();
        let block = BlockNumHash::new(15, chain.transactions.last().unwrap().1.block_hash.unwrap());
        assert_block_only_requests(&chain.rpc_requests, block);
    }
    // Persistence is sufficient after restart, and repeated confirmation reads no RPC.
    let reopened = ExecutorStore::new(fixture.db.clone(), fixture.view.clone(), 1).unwrap();
    assert_eq!(
        swap_order_state(
            &reopened
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == fixture.operation)
                .unwrap()
                .swap()
                .unwrap()
                .orders()[0]
        ),
        SwapOrderState::Done
    );
    fixture.chain.lock().unwrap().rpc_requests.clear();
    fixture
        .owner
        .observe_swap_settlement(fixture.operation, uid(1, 20), 15)
        .await
        .unwrap();
    assert!(fixture.chain.lock().unwrap().rpc_requests.is_empty());
    // Reuse after restart keeps completed order evidence local. A fresh current nonce
    // must clear both old hooks, but we need not attribute their historical execution.
    let restarted = restarted_owner(&fixture);
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 17;
        chain.nonces.push((15, 2));
    }
    assert!(
        restarted
            .reuse_swap_account(fixture.operation, 16)
            .await
            .is_err()
    );
    assert_eq!(fixture.record().nonce_observation(), nonce_before);
    fixture.chain.lock().unwrap().nonces.push((15, 3));
    fixture.chain.lock().unwrap().rpc_requests.clear();
    let reused = restarted
        .reuse_swap_account(fixture.operation, 16)
        .await
        .unwrap();
    assert_eq!(reused.expected_pre_hook_nonce(), U256::from(3));
    let checked = fixture.record();
    assert!(
        restarted
            .inputs_for_record(vec![sell_note(3)], &checked)
            .unwrap()
            .is_empty(),
        "completed swap inputs stay unavailable while private sync catches up"
    );
    assert!(
        checked.swap().unwrap().orders()[0]
            .observations()
            .shielded
            .is_none(),
        "current nonce admission must not fabricate a historical hook winner"
    );
    {
        let chain = fixture.chain.lock().unwrap();
        for request in &chain.rpc_requests {
            match request["method"].as_str().unwrap() {
                "eth_chainId" | "eth_blockNumber" | "eth_getCode" => {}
                "eth_getBlockByNumber" => assert_eq!(quantity(&request["params"][0]), 16),
                "eth_call" => {
                    assert_eq!(request["params"][0]["to"], json!(EXECUTOR));
                    let input = request["params"][0]["input"]
                        .as_str()
                        .or_else(|| request["params"][0]["data"].as_str())
                        .unwrap();
                    assert_eq!(
                        input,
                        alloy::hex::encode_prefixed(RelayAdapt7702::nonceCall::SELECTOR)
                    );
                }
                method => panic!(
                    "completed orders must not trigger historical or identifying order reads: {method}"
                ),
            }
        }
    }
    fixture.record_attempt(2, 30);
    assert_eq!(
        fixture.record().swap().unwrap().orders()[1]
            .pre_hook()
            .nonce(),
        U256::from(3)
    );
    restarted.shutdown().await;
    fixture.finish().await;
}

#[tokio::test]
async fn the_executed_fee_is_read_once_after_the_trade_and_kept_by_later_settlement_checks() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(1, 20);
    let order = uid(1, 20);
    let (url, requests, stub) = super::swap_order::spawn_bridge_stub(|_| {
        json!({"status": "fulfilled", "executedFee": "464572", "executedFeeToken": BUY}).to_string()
    })
    .await;
    let orderbook = crate::cow::CowOrderbookClient::new(
        crate::OperationHttpClient::for_tests(
            reqwest::Client::new(),
            crate::OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
        ),
        url,
        1,
    )
    .unwrap();
    let fee = |record: &ExecutorRecord| {
        record.swap().unwrap().orders()[0]
            .observations()
            .trade_amounts
            .map(|amounts| (amounts.executed_fee, amounts.executed_fee_token))
    };

    // Nothing is asked before the trade is verified.
    fixture
        .owner
        .observe_swap_executed_fee(fixture.operation, order, &orderbook)
        .await
        .unwrap();
    assert!(requests.lock().unwrap().is_empty());

    // A credit below the approved minimum leaves the order traded, so its settlement is
    // checked again later.
    let mut logs = settlement_logs(&fixture, 1, 20);
    logs[4].1 = post_hook_shield_log(1, 10);
    confirm_settlement(&fixture, logs).await;
    assert_eq!(state(&fixture.record(), 0), SwapOrderState::Traded);
    fixture
        .owner
        .observe_swap_executed_fee(fixture.operation, order, &orderbook)
        .await
        .unwrap();
    let charged = Some((Some(U256::from(464_572)), Some(BUY)));
    assert_eq!(fee(&fixture.record()), charged);
    assert_eq!(
        *requests.lock().unwrap(),
        vec![(format!("/api/api/v1/orders/{}", order.0), Value::Null)],
        "the fee read carries only the order UID"
    );

    // The next settlement check records the same trade again and keeps the fee, and neither
    // this owner nor one after a restart asks again.
    fixture
        .owner
        .observe_swap_settlement(fixture.operation, order, 15)
        .await
        .unwrap();
    assert_eq!(fee(&fixture.record()), charged);
    let restarted = restarted_owner(&fixture);
    for owner in [&fixture.owner, &restarted] {
        owner
            .observe_swap_executed_fee(fixture.operation, order, &orderbook)
            .await
            .unwrap();
    }
    assert_eq!(requests.lock().unwrap().len(), 1);
    stub.abort();
    restarted.shutdown().await;
    fixture.finish().await;
}

#[tokio::test]
async fn settlement_receipts_do_not_complete_an_early_or_copied_or_small_shield() {
    for scenario in 0..6 {
        let fixture = Fixture::start().await;
        fixture.record_attempt(1, 20);
        let mut logs = settlement_logs(&fixture, 1, 20);
        match scenario {
            0 => logs.rotate_right(3), // Funded post-hook before the trade and payout.
            1 => {
                logs[2].1 = Transfer {
                    from: Address::repeat_byte(0xab),
                    to: fixture.railgun,
                    value: U256::from(BUY_AMOUNT - 24),
                }
                .encode_log_data();
            }
            2 => logs[4].1 = post_hook_shield_log(1, 10), // Below the approved private minimum.
            3 => logs.insert(
                4,
                (fixture.railgun, post_hook_shield_log(2, BUY_AMOUNT - 24)),
            ), // Debit belonged to a different shield.
            4 => logs.push(logs[1].clone()), // Early funded hook followed by the actual payout.
            5 => {
                // A debit for the gross amount does not match this net private credit.
                logs[2].1 = Transfer {
                    from: EXECUTOR,
                    to: fixture.railgun,
                    value: U256::from(BUY_AMOUNT + 1),
                }
                .encode_log_data();
            }
            _ => unreachable!(),
        }
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 16;
            chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
            chain.rpc_requests.clear();
        }
        fixture
            .owner
            .observe_swap_settlement(fixture.operation, uid(1, 20), 15)
            .await
            .unwrap();
        let record = fixture.record();
        assert_eq!(
            swap_order_state(&record.swap().unwrap().orders()[0]),
            SwapOrderState::Traded,
            "scenario {scenario}"
        );
        assert!(fixture.reserved(&record));
        {
            let chain = fixture.chain.lock().unwrap();
            let block =
                BlockNumHash::new(15, chain.transactions.last().unwrap().1.block_hash.unwrap());
            assert_block_only_requests(&chain.rpc_requests, block);
        }
        fixture.finish().await;
    }
}

#[tokio::test]
async fn settlement_receipts_deliver_external_orders_from_the_trade_alone() {
    let receiver = Address::repeat_byte(0x77);
    let order = uid(1, 20);
    // An ERC-20 buy pays the receiver with a Transfer. A native buy's Trade reports `GPv2`'s
    // native buy address and emits no Transfer. Another order's trade settles nothing.
    for (buy, trade, matching) in [
        (BUY, trade_log_for(order, BUY), true),
        (Address::ZERO, trade_log_for(order, BUY_NATIVE_TOKEN), true),
        (BUY, trade_log_for(uid(2, 20), BUY), false),
    ] {
        let fixture = Fixture::start().await;
        fixture.record_delivery_attempt(1, 20, buy, SwapDelivery::External { receiver }, None);
        let mut logs = vec![(fixture.settlement, trade)];
        if buy == BUY {
            logs.push((
                BUY,
                Transfer {
                    from: fixture.settlement,
                    to: receiver,
                    value: U256::from(BUY_AMOUNT),
                }
                .encode_log_data(),
            ));
        }
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 16;
            chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
            chain.rpc_requests.clear();
        }
        fixture
            .owner
            .observe_swap_settlement(fixture.operation, order, 15)
            .await
            .unwrap();
        let record = fixture.record();
        let observed = record.swap().unwrap().orders()[0].observations();
        {
            let chain = fixture.chain.lock().unwrap();
            let block = chain.block(15);
            let traded = SwapObservation {
                block,
                transaction_hash: Some(chain.transactions.last().unwrap().0),
            };
            if matching {
                assert_eq!(
                    swap_order_state(&record.swap().unwrap().orders()[0]),
                    SwapOrderState::Done
                );
                assert_eq!(
                    (observed.traded, observed.delivered),
                    (Some(traded), Some(traded))
                );
                assert!(observed.settlement_credit.is_none());
            } else {
                assert_eq!(observed, SwapOrderObservations::default());
            }
            assert_block_only_requests(&chain.rpc_requests, block);
        }
        fixture.finish().await;
    }
}

/// Across delivery that reshields surplus, and the terms its post-hook's deposit signed. The
/// order's buy amount is 99 below the trade's, so the post-hook reshields 99.
fn across_order(spoke_pool: Address) -> (BridgeDelivery, AcrossOrderTerms) {
    (
        BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: 42161,
            receiver: Address::repeat_byte(0x77),
            destination_token: Address::repeat_byte(0xa0),
            surplus: BridgeSurplus::Reshield,
        },
        AcrossOrderTerms {
            spoke_pool,
            input_token: BUY,
            output_token: Address::repeat_byte(0xa0),
            input_amount: U256::from(BUY_AMOUNT - 99),
            output_amount: U256::from(9_800),
            quote_timestamp: 1_000_100,
            fill_deadline: 1_010_000,
            exclusive_relayer: Address::ZERO,
            exclusivity_parameter: 0,
        },
    )
}

/// The `FundsDeposited` event of the deposit an Across post-hook signed with `terms`.
fn signed_deposit(delivery: BridgeDelivery, terms: &AcrossOrderTerms) -> SpokePool::FundsDeposited {
    SpokePool::FundsDeposited {
        inputToken: address_to_bytes32(terms.input_token),
        outputToken: address_to_bytes32(terms.output_token),
        inputAmount: terms.input_amount,
        outputAmount: terms.output_amount,
        destinationChainId: U256::from(delivery.destination_chain),
        depositId: U256::from(42),
        quoteTimestamp: terms.quote_timestamp,
        fillDeadline: terms.fill_deadline,
        exclusivityDeadline: 0,
        depositor: address_to_bytes32(EXECUTOR),
        recipient: address_to_bytes32(delivery.receiver),
        exclusiveRelayer: address_to_bytes32(terms.exclusive_relayer),
        message: Bytes::new(),
    }
}

/// One settlement runs the pre-hook and pays the executor. Given its deposit, the Across
/// post-hook then runs, deposits, and reshields the surplus after the shield fee.
fn across_settlement_logs(
    fixture: &Fixture,
    terms: &AcrossOrderTerms,
    deposit: Option<SpokePool::FundsDeposited>,
) -> Vec<(Address, LogData)> {
    let transfer = |from: Address, to: Address, value: u64| {
        (
            BUY,
            Transfer {
                from,
                to,
                value: U256::from(value),
            }
            .encode_log_data(),
        )
    };
    let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
        .into_iter()
        .map(|data| (fixture.railgun, data))
        .collect::<Vec<_>>();
    logs.push((fixture.settlement, trade_log(uid(1, 20))));
    logs.push(transfer(fixture.settlement, EXECUTOR, BUY_AMOUNT));
    if let Some(deposit) = deposit {
        logs.extend([
            transfer(EXECUTOR, terms.spoke_pool, terms.input_amount.to()),
            (terms.spoke_pool, deposit.encode_log_data()),
            transfer(EXECUTOR, fixture.railgun, 74),
            // Railgun's treasury receives the fee separately.
            transfer(EXECUTOR, Address::repeat_byte(0xfe), 25),
            (fixture.railgun, post_hook_shield_log(1, 74)),
        ]);
    }
    logs
}

/// Record the settlement in block 15 from its receipts alone, and return the trade's
/// observation.
async fn confirm_settlement(fixture: &Fixture, logs: Vec<(Address, LogData)>) -> SwapObservation {
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 16;
        chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
        chain.rpc_requests.clear();
    }
    fixture
        .owner
        .observe_swap_settlement(fixture.operation, uid(1, 20), 15)
        .await
        .unwrap();
    let chain = fixture.chain.lock().unwrap();
    let block = chain.block(15);
    assert_block_only_requests(&chain.rpc_requests, block);
    SwapObservation {
        block,
        transaction_hash: Some(chain.transactions.last().unwrap().0),
    }
}

#[tokio::test]
async fn settlement_receipts_hand_off_to_across_only_with_the_signed_deposit() {
    // 0: the signed deposit, then the reshielded surplus. 1 and 2: a deposit to another
    // recipient, or of another output amount. 3: the post-hook didn't run. 4: the signed
    // deposit, paid by someone other than the executor.
    for scenario in 0..5 {
        let fixture = Fixture::start().await;
        let (delivery, terms) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
        fixture.record_delivery_attempt(
            1,
            20,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(terms)),
        );
        let mut deposit = signed_deposit(delivery, &terms);
        match scenario {
            1 => deposit.recipient = address_to_bytes32(Address::repeat_byte(0x78)),
            2 => deposit.outputAmount -= U256::ONE,
            _ => {}
        }
        let mut logs = across_settlement_logs(&fixture, &terms, (scenario != 3).then_some(deposit));
        if scenario == 4 {
            let executor_payment = Transfer {
                from: EXECUTOR,
                to: terms.spoke_pool,
                value: terms.input_amount,
            }
            .encode_log_data();
            logs.retain(|(_, data)| *data != executor_payment);
        }
        let traded = confirm_settlement(&fixture, logs).await;
        let record = fixture.record();
        let observed = record.swap().unwrap().orders()[0].observations();
        assert_eq!(observed.traded, Some(traded), "scenario {scenario}");
        if scenario != 0 {
            // The bought token stays in the executor.
            assert_eq!(
                state(&record, 0),
                SwapOrderState::Traded,
                "scenario {scenario}"
            );
            assert_eq!(
                (
                    observed.bridge_handoff,
                    observed.delivered,
                    observed.settlement_credit
                ),
                (None, None, None),
                "scenario {scenario}"
            );
            if scenario == 3 {
                // Explicit reconciliation finds the bought token still in the executor, which
                // recovery then offers.
                {
                    let mut chain = fixture.chain.lock().unwrap();
                    chain.nonces.push((15, 2));
                    chain.buy_balance.push((15, BUY_AMOUNT));
                }
                let record = fixture.observe(17, 14).await;
                assert_eq!(state(&record, 0), SwapOrderState::NotDelivered);
            }
            fixture.finish().await;
            continue;
        }
        // Handed off, and not yet delivered on the destination chain.
        assert_eq!(state(&record, 0), SwapOrderState::Bridging);
        assert_eq!(
            (observed.bridge_handoff, observed.delivered),
            (
                Some(SwapBridgeHandoff {
                    observation: traded,
                    deposit_id: Some(U256::from(42)),
                }),
                Some(traded)
            )
        );
        assert_eq!(
            observed
                .settlement_credit
                .map(|credit| (credit.private_amount, credit.fee)),
            Some((U256::from(74), Some(U256::from(25))))
        );
        // Explicit reconciliation finds the post-hook's nonce passed in the settlement, so the
        // retained deposit shows which payload took that nonce.
        fixture.chain.lock().unwrap().nonces.push((15, 3));
        let record = fixture.observe(17, 14).await;
        assert_eq!(
            record.swap().unwrap().orders()[0]
                .observations()
                .post_hook_deposit,
            Some(traded)
        );
        fixture.finish().await;
    }
}

/// Record a NEAR Intents order to BNB Chain and confirm its settlement in block 15, whose trade
/// pays `deposit_address`. Returns the trade's observation.
async fn near_handoff(fixture: &Fixture, deposit_address: Address) -> SwapObservation {
    fixture.record_delivery_attempt(
        1,
        20,
        BUY,
        SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::NearIntents,
            destination_chain: 56,
            receiver: Address::repeat_byte(0x77),
            destination_token: Address::ZERO,
            surplus: BridgeSurplus::BridgedByProvider,
        }),
        Some(BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
            deposit_address,
            min_amount_out: U256::from(15),
            amount_out: U256::from(16),
            deadline: "2026-09-30T01:00:00.000Z".into(),
            signed_quote: "{}".into(),
        })),
    );
    // The order's UID commits to the deposit address the settlement pays.
    let logs = vec![
        (fixture.settlement, trade_log(uid(1, 20))),
        (
            BUY,
            Transfer {
                from: fixture.settlement,
                to: deposit_address,
                value: U256::from(BUY_AMOUNT),
            }
            .encode_log_data(),
        ),
    ];
    confirm_settlement(fixture, logs).await
}

#[tokio::test]
async fn settlement_receipts_hand_off_to_near_intents_with_the_trade() {
    let fixture = Fixture::start().await;
    let traded = near_handoff(&fixture, Address::repeat_byte(0x79)).await;
    let record = fixture.record();
    let observed = record.swap().unwrap().orders()[0].observations();
    assert_eq!(state(&record, 0), SwapOrderState::Bridging);
    assert_eq!(
        (
            observed.bridge_handoff,
            observed.delivered,
            observed.settlement_credit
        ),
        (
            Some(SwapBridgeHandoff {
                observation: traded,
                deposit_id: None,
            }),
            Some(traded),
            None
        )
    );
    fixture.finish().await;
}

/// Bridge clients for the provider stub at `url`, on a direct test route.
fn bridge_clients(fixture: &Fixture, url: &url::Url) -> crate::SwapBridgeClients {
    let http = || {
        crate::OperationHttpClient::for_tests(
            reqwest::Client::new(),
            crate::OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
        )
    };
    crate::SwapBridgeClients {
        across: crate::bridge::AcrossClient::new(http(), url.clone()).unwrap(),
        near: crate::bridge::NearIntentsClient::new(
            http(),
            url.clone(),
            fixture
                .config
                .bridge_profile()
                .unwrap()
                .one_click_quote_key(),
        )
        .unwrap(),
    }
}

/// Arbitrum One, the Across orders' destination, on its own mock chain at head 30 with
/// finality depth 1.
async fn across_destination() -> (
    Arc<Mutex<MockChain>>,
    crate::settings::EffectiveChainConfig,
    tokio::task::JoinHandle<()>,
) {
    let chain = Arc::new(Mutex::new(MockChain {
        head: 30,
        ..MockChain::new(42161, Address::ZERO, Address::ZERO)
    }));
    let served = chain.clone();
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| served.lock().unwrap().respond(&request)),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let mut config =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(42161)
            .cloned()
            .unwrap();
    config.enabled = true;
    config.finality_depth = 1;
    config.rpc_route = crate::RpcChainRoute::new(42161, vec![endpoint]);
    (chain, config, server)
}

/// A relayer's fill of Across deposit `deposit_id` from chain 1 with the order's terms.
fn filled_relay(
    delivery: BridgeDelivery,
    terms: &AcrossOrderTerms,
    deposit_id: u64,
) -> SpokePool::FilledRelay {
    let recipient = address_to_bytes32(delivery.receiver);
    SpokePool::FilledRelay {
        inputToken: address_to_bytes32(terms.input_token),
        outputToken: address_to_bytes32(terms.output_token),
        inputAmount: terms.input_amount,
        outputAmount: terms.output_amount,
        repaymentChainId: U256::ONE,
        originChainId: U256::ONE,
        depositId: U256::from(deposit_id),
        fillDeadline: terms.fill_deadline,
        exclusivityDeadline: 0,
        exclusiveRelayer: B256::ZERO,
        relayer: address_to_bytes32(Address::repeat_byte(0x55)),
        depositor: address_to_bytes32(EXECUTOR),
        recipient,
        messageHash: B256::ZERO,
        relayExecutionInfo: V3RelayExecutionEventInfo {
            updatedRecipient: recipient,
            updatedMessageHash: B256::ZERO,
            updatedOutputAmount: terms.output_amount,
            fillType: 0,
        },
    }
}

async fn observe_bridge(
    fixture: &Fixture,
    clients: &crate::SwapBridgeClients,
    destination: &crate::settings::EffectiveChainConfig,
) -> Option<SwapBridgeOutcome> {
    fixture
        .owner
        .observe_swap_bridge(fixture.operation, uid(1, 20), clients, destination)
        .await
        .unwrap()
}

#[tokio::test]
async fn across_delivery_is_verified_from_finalized_destination_receipts() {
    // 0: the deposit's fill. 1: a fill of another deposit. 2: the fill's block is reorged
    // during the read. 3: Across reports the deposit expired. 4: a slow fill, which pays more
    // than the signed output. 5: a fill that executes less than the signed output.
    for scenario in 0..6 {
        let fixture = Fixture::start().await;
        let (delivery, terms) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
        fixture.record_delivery_attempt(
            1,
            20,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(terms)),
        );
        let logs = across_settlement_logs(&fixture, &terms, Some(signed_deposit(delivery, &terms)));
        confirm_settlement(&fixture, logs).await;
        let (destination, config, server) = across_destination().await;
        let spoke_pool = config.bridge_profile().unwrap().spoke_pool();
        let mut fill = filled_relay(delivery, &terms, if scenario == 1 { 43 } else { 42 });
        let executed = match scenario {
            4 => {
                fill.relayExecutionInfo.fillType = 2;
                terms.output_amount + U256::from(50)
            }
            5 => terms.output_amount - U256::ONE,
            _ => terms.output_amount,
        };
        fill.relayExecutionInfo.updatedOutputAmount = executed;
        {
            let mut chain = destination.lock().unwrap();
            chain.add_addressed_transaction(
                30,
                spoke_pool,
                Bytes::new(),
                vec![(spoke_pool, fill.encode_log_data())],
            );
            chain.reorg_on_receipts = scenario == 2;
        }
        // Across's fill transaction, amount and recipient only locate the block.
        let status = if scenario == 3 { "expired" } else { "filled" };
        let deposit = format!(
            r#"{{"deposit":{{"status":"{status}","fillBlockNumber":30,"fillTx":"{}","outputAmount":"1","recipient":"{}","destinationChainId":"42161"}}}}"#,
            B256::repeat_byte(0xf1),
            Address::repeat_byte(0x78),
        );
        let (url, lookups, stub) =
            super::swap_order::spawn_bridge_stub(move |_| deposit.clone()).await;
        let clients = bridge_clients(&fixture, &url);
        let first = observe_bridge(&fixture, &clients, &config).await;
        let outcome = if scenario == 3 {
            assert_eq!(first, Some(SwapBridgeOutcome::Refunding));
            assert!(destination.lock().unwrap().rpc_requests.is_empty());
            first
        } else {
            // The fill's block isn't final at head 30.
            assert_eq!(first, None, "scenario {scenario}");
            destination.lock().unwrap().head = 31;
            let outcome = observe_bridge(&fixture, &clients, &config).await;
            let chain = destination.lock().unwrap();
            let (transaction_hash, transaction, _) = &chain.transactions[0];
            let block = BlockNumHash::new(30, transaction.block_hash.unwrap());
            // The executed amount is delivered.
            assert_eq!(
                outcome,
                matches!(scenario, 0 | 4).then_some(SwapBridgeOutcome::DeliveredVerified {
                    block,
                    transaction_hash: *transaction_hash,
                    output_amount: executed,
                }),
                "scenario {scenario}"
            );
            assert_block_only_requests(&chain.rpc_requests, block);
            outcome
        };
        let record = fixture.record();
        assert_eq!(
            record.swap().unwrap().orders()[0]
                .observations()
                .bridge_outcome,
            outcome
        );
        assert_eq!(
            state(&record, 0),
            [
                SwapOrderState::Done,
                SwapOrderState::Bridging,
                SwapOrderState::Bridging,
                SwapOrderState::Refunding,
                SwapOrderState::Done,
                SwapOrderState::Bridging,
            ][scenario],
            "scenario {scenario}"
        );
        assert!(
            lookups
                .lock()
                .unwrap()
                .iter()
                .all(|(path, _)| path == "/api/deposit?originChainId=1&depositId=42")
        );
        stub.abort();
        server.abort();
        fixture.finish().await;
    }
}

/// Across can fill a deposit after reporting it expired. Routine polling stops at the refund,
/// but an explicit status check asks Across again and records the verified fill.
#[tokio::test]
async fn an_explicit_check_corrects_an_across_refund_with_a_verified_fill() {
    let fixture = Fixture::start().await;
    let (delivery, terms) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
    fixture.record_delivery_attempt(
        1,
        20,
        BUY,
        SwapDelivery::Bridge(delivery),
        Some(BridgeOrderTerms::Across(terms)),
    );
    let logs = across_settlement_logs(&fixture, &terms, Some(signed_deposit(delivery, &terms)));
    confirm_settlement(&fixture, logs).await;
    let (destination, config, server) = across_destination().await;
    let spoke_pool = config.bridge_profile().unwrap().spoke_pool();
    {
        let mut chain = destination.lock().unwrap();
        chain.add_addressed_transaction(
            30,
            spoke_pool,
            Bytes::new(),
            vec![(
                spoke_pool,
                filled_relay(delivery, &terms, 42).encode_log_data(),
            )],
        );
        chain.head = 31;
    }
    let deposit = |status: &str| {
        format!(
            r#"{{"deposit":{{"status":"{status}","fillBlockNumber":30,"outputAmount":"1","recipient":"{}","destinationChainId":"42161"}}}}"#,
            delivery.receiver,
        )
    };
    let reply = Arc::new(Mutex::new(deposit("expired")));
    let served = reply.clone();
    let (url, lookups, stub) =
        super::swap_order::spawn_bridge_stub(move |_| served.lock().unwrap().clone()).await;
    let clients = bridge_clients(&fixture, &url);
    assert_eq!(
        observe_bridge(&fixture, &clients, &config).await,
        Some(SwapBridgeOutcome::Refunding)
    );
    *reply.lock().unwrap() = deposit("filled");
    assert_eq!(
        observe_bridge(&fixture, &clients, &config).await,
        Some(SwapBridgeOutcome::Refunding)
    );
    assert_eq!(lookups.lock().unwrap().len(), 1);
    assert_eq!(fixture.record().swap_bridges_to_track().count(), 0);
    let outcome = fixture
        .owner
        .check_swap_bridge(fixture.operation, uid(1, 20), &clients, &config)
        .await
        .unwrap();
    let expected = {
        let chain = destination.lock().unwrap();
        let (transaction_hash, transaction, _) = &chain.transactions[0];
        SwapBridgeOutcome::DeliveredVerified {
            block: BlockNumHash::new(30, transaction.block_hash.unwrap()),
            transaction_hash: *transaction_hash,
            output_amount: terms.output_amount,
        }
    };
    assert_eq!(outcome, Some(expected));
    let record = fixture.record();
    assert_eq!(
        record.swap().unwrap().orders()[0]
            .observations()
            .bridge_outcome,
        Some(expected)
    );
    assert_eq!(state(&record, 0), SwapOrderState::Done);
    stub.abort();
    server.abort();
    fixture.finish().await;
}

/// Across refunds an expired deposit to the stealth account, which may also hold kept surplus
/// larger than the deposit, so its balance can't show the refund. Routine polling doesn't look
/// the refund up. An explicit status check verifies it in the finalized receipts of the block
/// holding the refund transaction Across names: a transfer of the deposit's input token from
/// the `SpokePool` to the stealth account covering the deposit. The verified refund survives a
/// restart and reconciliation, and a reorg of its block removes it.
#[tokio::test]
async fn an_explicit_check_verifies_an_across_refund_on_this_chain() {
    let refund_of = |record: &ExecutorRecord| {
        record.swap().unwrap().orders()[0]
            .observations()
            .bridge_refund
    };
    let arbitrum =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(42161)
            .cloned()
            .unwrap();
    // 0: the refund. 1: from another sender. 2: of another token. 3: to another account. 4: less
    // than the deposit.
    for scenario in 0..5 {
        let fixture = Fixture::start().await;
        let (delivery, terms) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
        let delivery = BridgeDelivery {
            surplus: BridgeSurplus::KeepInAccount,
            ..delivery
        };
        let terms = AcrossOrderTerms {
            input_amount: U256::from(4_000),
            ..terms
        };
        fixture.record_delivery_attempt(
            1,
            20,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(terms)),
        );
        let mut logs =
            across_settlement_logs(&fixture, &terms, Some(signed_deposit(delivery, &terms)));
        // The post-hook keeps the surplus of 5,999 rather than shielding it.
        logs.truncate(logs.len() - 3);
        confirm_settlement(&fixture, logs).await;
        let mut refund = Transfer {
            from: terms.spoke_pool,
            to: EXECUTOR,
            value: terms.input_amount,
        };
        let mut token = BUY;
        match scenario {
            1 => refund.from = Address::repeat_byte(0x99),
            2 => token = OTHER_BUY,
            3 => refund.to = Address::repeat_byte(0x99),
            4 => refund.value -= U256::ONE,
            _ => {}
        }
        let refund_tx = {
            let mut chain = fixture.chain.lock().unwrap();
            chain.add_addressed_transaction(
                18,
                terms.spoke_pool,
                Bytes::new(),
                vec![(token, refund.encode_log_data())],
            );
            chain.head = 20;
            chain.rpc_methods.clear();
            chain.transactions.last().unwrap().0
        };
        let deposit = format!(
            r#"{{"deposit":{{"status":"expired","depositRefundTxHash":"{refund_tx}","outputAmount":"1","recipient":"{}","destinationChainId":"42161"}}}}"#,
            delivery.receiver,
        );
        let (url, _, stub) = super::swap_order::spawn_bridge_stub(move |_| deposit.clone()).await;
        let clients = bridge_clients(&fixture, &url);
        assert_eq!(
            observe_bridge(&fixture, &clients, &arbitrum).await,
            Some(SwapBridgeOutcome::Refunding)
        );
        let looked_up = |fixture: &Fixture| {
            fixture
                .chain
                .lock()
                .unwrap()
                .rpc_methods
                .iter()
                .any(|method| method == "eth_getTransactionReceipt")
        };
        assert!(!looked_up(&fixture));
        assert_eq!(refund_of(&fixture.record()), None);
        assert_eq!(
            fixture
                .owner
                .check_swap_bridge(fixture.operation, uid(1, 20), &clients, &arbitrum)
                .await
                .unwrap(),
            Some(SwapBridgeOutcome::Refunding)
        );
        assert!(looked_up(&fixture));
        let verified = SwapObservation {
            block: fixture.chain.lock().unwrap().block(18),
            transaction_hash: Some(refund_tx),
        };
        let record = fixture.record();
        assert_eq!(
            refund_of(&record),
            (scenario == 0).then_some(verified),
            "scenario {scenario}"
        );
        assert_eq!(state(&record, 0), SwapOrderState::Refunding);
        if scenario == 0 {
            // A verified refund rules out a fill, and isn't replaced.
            assert!(matches!(
                fixture.store.record_swap_bridge_outcome(
                    fixture.operation,
                    uid(1, 20),
                    SwapBridgeOutcome::DeliveredVerified {
                        block: BlockNumHash::new(30, B256::repeat_byte(30)),
                        transaction_hash: B256::repeat_byte(31),
                        output_amount: terms.output_amount,
                    },
                ),
                Err(ExecutorStoreError::InvalidRecord)
            ));
            assert!(matches!(
                fixture.store.record_swap_bridge_refund(
                    fixture.operation,
                    uid(1, 20),
                    SwapObservation {
                        block: fixture.chain.lock().unwrap().block(19),
                        ..verified
                    },
                ),
                Err(ExecutorStoreError::InvalidRecord)
            ));
            let restarted =
                ExecutorStore::new(fixture.db.clone(), fixture.view.clone(), 1).unwrap();
            let restored = restarted
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == fixture.operation)
                .unwrap();
            assert_eq!(refund_of(&restored), Some(verified));
            drop(restarted);
            // The post-hook's nonce passed in the settlement.
            fixture.chain.lock().unwrap().nonces.push((15, 3));
            assert_eq!(refund_of(&fixture.observe(20, 14).await), Some(verified));
            fixture.chain.lock().unwrap().reorg(18);
            assert_eq!(refund_of(&fixture.observe(21, 14).await), None);
        }
        stub.abort();
        fixture.finish().await;
    }
}

#[tokio::test]
async fn near_intents_reports_set_the_outcome_and_final_ones_stop_tracking() {
    let destination_tx = B256::repeat_byte(2);
    let bnb =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(56)
            .cloned()
            .unwrap();
    for (status, expected, expected_state) in [
        (
            "SUCCESS",
            Some(SwapBridgeOutcome::DeliveredReported {
                amount_out: Some(U256::from(16)),
                transaction_hash: Some(destination_tx),
            }),
            SwapOrderState::Done,
        ),
        (
            "REFUNDED",
            Some(SwapBridgeOutcome::Refunding),
            SwapOrderState::Refunding,
        ),
        (
            "FAILED",
            Some(SwapBridgeOutcome::NeedsAttention),
            SwapOrderState::NeedsAttention,
        ),
        (
            "INCOMPLETE_DEPOSIT",
            Some(SwapBridgeOutcome::NeedsAttention),
            SwapOrderState::NeedsAttention,
        ),
        ("PROCESSING", None, SwapOrderState::Bridging),
    ] {
        let fixture = Fixture::start().await;
        near_handoff(&fixture, Address::repeat_byte(0x79)).await;
        let report = format!(
            r#"{{"status":"{status}","swapDetails":{{"amountOut":"16","destinationChainTxHashes":[{{"hash":"{destination_tx}","explorerUrl":"https://bscscan.com/tx/{destination_tx}"}}]}}}}"#
        );
        let (url, requests, stub) =
            super::swap_order::spawn_bridge_stub(move |_| report.clone()).await;
        let clients = bridge_clients(&fixture, &url);
        assert_eq!(
            observe_bridge(&fixture, &clients, &bnb).await,
            expected,
            "{status}"
        );
        let record = fixture.record();
        assert_eq!(
            record.swap().unwrap().orders()[0]
                .observations()
                .bridge_outcome,
            expected,
            "{status}"
        );
        assert_eq!(state(&record, 0), expected_state, "{status}");
        // Only an order without an outcome is polled again.
        assert_eq!(
            record.swap_bridges_to_track().count(),
            usize::from(expected.is_none()),
            "{status}"
        );
        if expected == Some(SwapBridgeOutcome::NeedsAttention) {
            // Routine polling leaves it alone; an explicit status check asks again.
            assert_eq!(observe_bridge(&fixture, &clients, &bnb).await, expected);
            assert_eq!(requests.lock().unwrap().len(), 1);
            assert_eq!(
                fixture
                    .owner
                    .check_swap_bridge(fixture.operation, uid(1, 20), &clients, &bnb)
                    .await
                    .unwrap(),
                expected
            );
            assert_eq!(requests.lock().unwrap().len(), 2);
        }
        stub.abort();
        fixture.finish().await;
    }
}

#[tokio::test]
async fn settlement_receipt_failures_and_reorgs_preserve_pending_state_without_fallback() {
    for scenario in 0..5 {
        let fixture = Fixture::start().await;
        fixture.record_attempt(1, 20);
        let mut logs = settlement_logs(&fixture, 1, 20);
        if scenario == 4 {
            logs[0].1 = trade_log(uid(2, 20));
        }
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 16;
            chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
            match scenario {
                0 => chain.receipt_error = Some(-32601),
                1 => chain.transactions.last_mut().unwrap().2.block_hash = Some(B256::ZERO),
                2 => chain.reorg_on_receipts = true,
                3 => {
                    chain.transactions.last_mut().unwrap().2.inner = ReceiptEnvelope::Eip1559(
                        Receipt {
                            status: Eip658Value::Eip658(false),
                            cumulative_gas_used: 1,
                            logs: Vec::new(),
                        }
                        .with_bloom(),
                    );
                }
                4 => {}
                _ => unreachable!(),
            }
            chain.rpc_requests.clear();
        }
        let result = fixture
            .owner
            .observe_swap_settlement(fixture.operation, uid(1, 20), 15)
            .await;
        assert_eq!(result.is_err(), scenario < 3);
        let record = fixture.record();
        assert!(
            record.swap().unwrap().orders()[0]
                .observations()
                .traded
                .is_none()
        );
        assert!(fixture.reserved(&record));
        {
            let chain = fixture.chain.lock().unwrap();
            let block =
                BlockNumHash::new(15, chain.transactions.last().unwrap().1.block_hash.unwrap());
            assert_block_only_requests(&chain.rpc_requests, block);
        }
        fixture.finish().await;
    }
}

#[tokio::test]
async fn settlement_receipts_fail_over_to_another_whole_block_provider() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(1, 20);
    let logs = settlement_logs(&fixture, 1, 20);
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 16;
        chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
        chain.rpc_requests.clear();
    }
    let failed_receipts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let failed = failed_receipts.clone();
    let served = fixture.chain.clone();
    let (unsupported, unsupported_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| {
            let result = served.lock().unwrap().respond(&request);
            if request["method"] == "eth_getBlockReceipts" {
                failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":-32601, "message":"unsupported"}})
            } else { result }
        }), Arc::default(), Arc::default(),
    ).await;
    let served = fixture.chain.clone();
    let (working, working_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| served.lock().unwrap().respond(&request)),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let mut config =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    config.finality_depth = 1;
    config.rpc_route = crate::RpcChainRoute::new(1, vec![unsupported, working]);
    let owner = ExecutorOwner::new(
        0,
        fixture.db.clone(),
        fixture.view.clone(),
        config,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    owner
        .observe_swap_settlement(fixture.operation, uid(1, 20), 15)
        .await
        .unwrap();
    assert_eq!(
        failed_receipts.load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(
        swap_order_state(&fixture.record().swap().unwrap().orders()[0]),
        SwapOrderState::Done
    );
    {
        let chain = fixture.chain.lock().unwrap();
        assert_block_only_requests(&chain.rpc_requests, chain.block(15));
    }
    owner.shutdown().await;
    unsupported_server.abort();
    working_server.abort();
    fixture.finish().await;
}
