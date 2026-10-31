//! Swap observations from canonical chain data served by a mock RPC, including
//! reorgs that remove the evidence an observation or a reservation release rests on.

use super::*;
use crate::vault::{
    SwapAccountRefusal, SwapAccountRole, SwapAccountUse, SwapAdmissionEvidence, SwapSpeedUp,
    swap_account_refusal,
};
use crate::{SpeedUpBlock, SwapOrderState, swap_attempt_state, swap_order_state};
use alloy::consensus::Transaction as _;
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
pub(super) const EXECUTOR: Address = address!("e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0");
const SELL_AMOUNT: u64 = 10_000;
const BUY_AMOUNT: u64 = 9_999;
const PRE_HOOK_NULLIFIER: u8 = 0x21;
const PRE_HOOK_COMMITMENT: u8 = 0x22;

alloy::sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Trade(address indexed owner, address sellToken, address buyToken, uint256 sellAmount, uint256 buyAmount, uint256 feeAmount, bytes orderUid);
    function filledAmount(bytes orderUid) external view returns (uint256);
}

fn swap_bounds() -> SwapApprovedBounds {
    SwapApprovedBounds {
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
        destination_shield_fee_bps: None,
        delivery_allowance: None,
        destination_setup_fee: None,
        source_setup_fee: None,
    }
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
pub(super) struct MockChain {
    chain_id: u64,
    railgun: Address,
    delegate: Address,
    pub(super) head: u64,
    reorgs: Vec<u64>,
    pub(super) nonces: Vec<(u64, u64)>,
    pub(super) buy_balance: Vec<(u64, u64)>,
    /// Per order, the block from which the settlement reads its fill as zero, as
    /// after it frees an expired order's storage.
    fill_cleared: Vec<(OrderUid, u64)>,
    /// Per order, the block from which the settlement reads its fill as the maximum,
    /// as after `invalidateOrder`.
    pub(super) invalidated: Vec<(OrderUid, u64)>,
    logs: Vec<Log>,
    pub(super) transactions: Vec<(B256, alloy::rpc::types::Transaction, TransactionReceipt)>,
    log_queries: usize,
    rpc_methods: Vec<String>,
    pub(super) rpc_requests: Vec<Value>,
    pub(super) receipt_error: Option<i64>,
    reorg_on_receipts: bool,
    /// The block from which the swap's pre-hook notes read as spent, as after a settlement
    /// ran a pre-hook.
    nullifiers_spent: Option<u64>,
    /// What `depositQuoteTimeBuffer()` returns. `None` reverts the call.
    pub(super) quote_time_buffer: Option<u32>,
    /// The executor's code at the head block in place of the accepted delegation, when set.
    head_code: Option<Bytes>,
}

impl MockChain {
    pub(super) const fn new(chain_id: u64, railgun: Address, delegate: Address) -> Self {
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
            nullifiers_spent: None,
            quote_time_buffer: None,
            head_code: None,
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

    pub(super) fn block(&self, number: u64) -> BlockNumHash {
        BlockNumHash::new(number, self.hash(number))
    }

    pub(super) fn reorg(&mut self, from: u64) {
        self.reorgs.push(from);
    }

    fn number_of(&self, id: &Value) -> Option<u64> {
        let hash: B256 = serde_json::from_value(id.get("blockHash").unwrap_or(id).clone()).ok()?;
        (0..=self.head).find(|number| self.hash(*number) == hash)
    }

    fn canonical(&self, number: Option<u64>, hash: Option<B256>) -> bool {
        number.is_some_and(|number| number <= self.head && hash == Some(self.hash(number)))
    }

    /// How many of `from`'s transactions canonical blocks hold up to the block `id` names by
    /// its hash: the account's nonce there. `None` for a block that isn't canonical.
    pub(super) fn transaction_count_at(&self, from: Address, id: &Value) -> Option<u64> {
        let number = self.number_of(id)?;
        let count = self
            .transactions
            .iter()
            .filter(|(_, transaction, _)| {
                transaction.inner.signer() == from
                    && transaction
                        .block_number
                        .is_some_and(|block| block <= number)
                    && self.canonical(transaction.block_number, transaction.block_hash)
            })
            .count();
        Some(u64::try_from(count).unwrap())
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
    pub(super) fn add_transaction(
        &mut self,
        number: u64,
        to: Address,
        input: Bytes,
        logs: Vec<LogData>,
    ) {
        self.add_addressed_transaction(
            number,
            to,
            input,
            logs.into_iter().map(|data| (self.railgun, data)).collect(),
        );
    }

    pub(super) fn add_addressed_transaction(
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
        self.include(
            number,
            TxEnvelope::Eip1559(signed),
            Address::repeat_byte(0xbb),
            true,
            logs,
        );
    }

    /// `transaction` from `from`, included in block `number` with `status` and `logs`.
    pub(super) fn include(
        &mut self,
        number: u64,
        transaction: TxEnvelope,
        from: Address,
        status: bool,
        logs: Vec<(Address, LogData)>,
    ) {
        let hash = *transaction.tx_hash();
        let to = transaction.to();
        let block_hash = self.hash(number);
        self.add_logs(number, hash, logs);
        let receipt_logs = self
            .logs
            .iter()
            .filter(|log| {
                log.transaction_hash == Some(hash)
                    && log.block_number == Some(number)
                    && log.block_hash == Some(block_hash)
            })
            .cloned()
            .collect();
        let receipt = TransactionReceipt {
            inner: ReceiptEnvelope::Eip1559(
                Receipt {
                    status: Eip658Value::Eip658(status),
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
            to,
            contract_address: None,
        };
        let transaction = alloy::rpc::types::Transaction {
            inner: Recovered::new_unchecked(transaction, from),
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

    pub(super) fn respond(&mut self, request: &Value) -> Value {
        self.rpc_methods
            .push(request["method"].as_str().unwrap().to_owned());
        self.rpc_requests.push(request.clone());
        let params = &request["params"];
        let result = match request["method"].as_str().unwrap() {
            "eth_chainId" => json!(format!("0x{:x}", self.chain_id)),
            "eth_blockNumber" => json!(format!("0x{:x}", self.head)),
            "eth_getBlockByNumber" => {
                let number = if params[0] == "latest" {
                    self.head
                } else {
                    quantity(&params[0])
                };
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
                let at_head = self.number_of(&params[1]) == Some(self.head);
                let code = self
                    .head_code
                    .clone()
                    .filter(|_| at_head)
                    .unwrap_or_else(|| {
                        [
                            EIP7702_DELEGATION_DESIGNATOR.as_slice(),
                            self.delegate.as_slice(),
                        ]
                        .concat()
                        .into()
                    });
                serde_json::to_value(code).unwrap()
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
                    // Only a test that says so spends the swap's notes.
                    U256::from(self.nullifiers_spent.is_some_and(|from| from <= number))
                } else if input[..4] == keccak256("depositQuoteTimeBuffer()")[..4] {
                    let Some(buffer) = self.quote_time_buffer else {
                        return json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "execution reverted"}});
                    };
                    U256::from(buffer)
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

pub(super) fn private_transaction(nullifier: u8, commitment: u8) -> Transaction {
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
pub(super) fn private_logs(nullifier: u8, commitment: u8) -> Vec<LogData> {
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

pub(super) fn execute(transactions: Vec<Transaction>, calls: Vec<Call>, nonce: u64) -> Bytes {
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

    /// The swap executor of [`Self::start`] whose setup is in the head block 14, whose nonce
    /// no block consumes yet: reconciled at confirmed block 13, the setup stays unresolved at
    /// nonce 0.
    async fn start_pending() -> Self {
        Self::start_with("Private swap", 14, false).await
    }

    async fn start_for_purpose(purpose: &str) -> Self {
        Self::start_with(purpose, 11, true).await
    }

    /// The setup's transaction is in `setup_block`, which consumes its nonce if `resolves`.
    async fn start_with(purpose: &str, setup_block: u64, resolves: bool) -> Self {
        let nonces = if resolves {
            vec![(setup_block, 1)]
        } else {
            Vec::new()
        };
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
            nonces,
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
        // The swap owner's purpose summary marks the record as a swap executor. A swap reserves
        // its account with the terms the user approved, which its first use holds.
        let approval = (purpose == "Private swap").then(|| SwapApproval {
            bounds: swap_bounds(),
            price_verified: None,
            price_acknowledged: false,
            delivery: SwapDelivery::Reshield,
            tokens: Some(SwapApprovalTokens {
                sell: SELL,
                buy: BUY,
            }),
            accounts: None,
        });
        store
            .reserve_with_swap_approval(
                operation,
                delegate,
                Some(purpose),
                &[ExecutorAsset::Erc20(SELL), ExecutorAsset::Erc20(BUY)],
                approval,
                None,
            )
            .unwrap();
        store.bind_address(operation, EXECUTOR).unwrap();
        let before_setup =
            ExecutorNonceObservation::new(BlockNumHash::new(10, B256::ZERO), U256::ZERO);
        store.record_account_read(operation, before_setup).unwrap();
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
        chain.lock().unwrap().add_transaction(
            setup_block,
            EXECUTOR,
            setup_call,
            private_logs(0x11, 0x12),
        );
        let reconciled = owner
            .reconcile_account(operation, crate::OperationHttpClient::direct_for_tests())
            .await
            .unwrap();
        assert_eq!(
            reconciled.record().payload_state(setup),
            Some(if resolves {
                ExecutorPayloadState::Resolved
            } else {
                ExecutorPayloadState::Pending
            })
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
        let bounds = SwapApprovedBounds {
            post_hook_gas_limit: post_hook.then_some(1),
            destination_minimum: match &bridge {
                Some(BridgeOrderTerms::Across(terms)) => Some(terms.output_amount),
                Some(BridgeOrderTerms::NearIntents(terms)) => Some(terms.min_amount_out),
                None => None,
            },
            ..swap_bounds()
        };
        // A private Bridge order is recorded only for a use that names the destination
        // stealth account serving it. The use the swap reserved its account with names none,
        // so it is cancelled before the account is claimed again. An account no swap claims
        // takes the order's use through a claim too.
        let private = match delivery {
            SwapDelivery::Bridge(bridge) if bridge.is_private() => Some(bridge),
            _ => None,
        };
        let reserved = self.record().active_swap_use();
        if private.is_some() || reserved.is_none() {
            let id = match reserved {
                Some(reserved) => {
                    self.store
                        .cancel_swap_use(self.operation, reserved)
                        .unwrap();
                    SwapUseId::random().unwrap()
                }
                None => SwapUseId::first(self.operation),
            };
            let destination =
                private.map(|bridge| (bridge, ExecutorOperationId::random().unwrap()));
            self.store
                .claim_swap_pair(crate::vault::SwapPairClaim {
                    id,
                    source: crate::vault::SwapAccountChoice::Existing(self.operation),
                    delegate: self.delegate,
                    purpose_summary: None,
                    assets: Vec::new(),
                    approval: SwapApproval {
                        bounds: bounds.clone(),
                        price_verified: None,
                        price_acknowledged: false,
                        delivery: delivery.clone(),
                        tokens: Some(SwapApprovalTokens { sell: SELL, buy }),
                        accounts: None,
                    },
                    destination: destination.map(|(private, operation)| {
                        crate::vault::SwapDestinationClaim {
                            chain_id: private.destination_chain,
                            account: crate::vault::SwapAccountChoice::New(operation),
                            delegate: self.delegate,
                            destination_token: private.destination_token,
                        }
                    }),
                })
                .unwrap();
            if let Some((private, operation)) = destination {
                ExecutorStore::new(
                    self.db.clone(),
                    self.view.clone(),
                    private.destination_chain,
                )
                .unwrap()
                .bind_address(operation, private.receiver)
                .unwrap();
            }
        }
        self.store
            .record_swap_attempt(
                self.operation,
                self.attempt(attempt, valid_to_block, buy, delivery, bounds, bridge),
            )
            .unwrap();
    }

    /// The attempt [`Self::record_delivery_attempt`] records, signed at the record's
    /// reconciled nonce.
    fn attempt(
        &self,
        attempt: u8,
        valid_to_block: u64,
        buy: Address,
        delivery: SwapDelivery,
        bounds: SwapApprovedBounds,
        bridge: Option<BridgeOrderTerms>,
    ) -> SwapAttempt {
        self.attempt_after(attempt, valid_to_block, buy, delivery, bounds, bridge, 0)
    }

    /// [`Self::attempt`] with its hooks signed `offset` nonces past the reconciled one, as an
    /// order on an included setup takes the nonce after the setup's. The hooks still name the
    /// record's observation.
    #[allow(clippy::too_many_arguments)]
    fn attempt_after(
        &self,
        attempt: u8,
        valid_to_block: u64,
        buy: Address,
        delivery: SwapDelivery,
        bounds: SwapApprovedBounds,
        bridge: Option<BridgeOrderTerms>,
        offset: u64,
    ) -> SwapAttempt {
        let post_hook = delivery.has_post_hook();
        let mut shield = post_hook_shield(attempt);
        shield.preimage.token = TokenData::erc20(buy);
        let observed = self.record().nonce_observation().unwrap();
        let nonce = observed.nonce().to::<u64>() + offset;
        let inputs = vec![self.input.clone()];
        SwapAttempt {
            // The use that claims the account.
            use_id: self.record().active_swap_use().unwrap(),
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
            bounds,
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
            speed_up: None,
        }
    }

    /// Observe up to the head's confirmed block, with finality depth 1.
    async fn observe(&self, head: u64, start: u64) -> ExecutorRecord {
        self.chain.lock().unwrap().head = head;
        self.owner
            .observe_swap(
                self.operation,
                start..head,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap()
            .record()
            .clone()
    }

    /// [`Self::observe`] with `sync` as private sync's snapshot.
    async fn observe_synced(
        &self,
        head: u64,
        start: u64,
        sync: Arc<sync_service::WalletCurrentSnapshot>,
    ) -> ExecutorRecord {
        self.chain.lock().unwrap().head = head;
        self.owner
            .observe_swap_synced(
                self.operation,
                start..head,
                Some(sync),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap()
            .record()
            .clone()
    }

    /// Observe `range` at the current head. Unlike [`Self::observe`], the page may
    /// end before the confirmed block.
    async fn observe_page(&self, range: std::ops::Range<u64>) -> ExecutorRecord {
        self.owner
            .observe_swap(
                self.operation,
                range,
                crate::OperationHttpClient::direct_for_tests(),
            )
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

/// Private sync's snapshot holding `notes`, scanned past every block the tests observe.
fn synced(notes: Vec<railgun_wallet::WalletUtxo>) -> Arc<sync_service::WalletCurrentSnapshot> {
    sync_service::WalletCurrentSnapshot::new(
        1_000,
        0,
        0,
        notes,
        sync_service::WalletPendingOverlay::default(),
    )
}

fn state(record: &ExecutorRecord, attempt: usize) -> SwapOrderState {
    let swap = record.swap().unwrap();
    swap_order_state(swap, &swap.orders()[attempt])
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
        .swap_order_preview(fixture.operation, false, None)
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
            .refresh_swap_executor(
                &mut review,
                15,
                SwapAccountUse::Claimed(crate::vault::SwapUseId::first(fixture.operation)),
                crate::OperationHttpClient::direct_for_tests(),
            )
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
    let Some(railgun_wallet::tx::CompositeAdapter::Executor(context)) = &mut changed_nonce.adapter
    else {
        panic!("the pre-hook request names its executor");
    };
    context.execution_nonce += U256::ONE;
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
            .refresh_swap_executor(
                &mut review,
                15,
                SwapAccountUse::Claimed(crate::vault::SwapUseId::first(fixture.operation)),
                crate::OperationHttpClient::direct_for_tests(),
            )
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
        .reuse_swap_account(
            fixture.operation,
            13,
            SwapAccountRole::Source,
            SwapAccountUse::New,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(selected.executor(), EXECUTOR);
    assert!(selected.is_reused());
    fixture.record_attempt(0, 28);
    assert!(
        listed(&fixture.owner, fixture.operation).is_none(),
        "an order that can still execute keeps the account out of the list"
    );
    // The same order keeps it from either role, with the reason the form shows.
    for role in [
        SwapAccountRole::Source,
        SwapAccountRole::Destination { token: OTHER_BUY },
    ] {
        assert_eq!(
            fixture
                .owner
                .swap_account_refusal(fixture.operation, role)
                .unwrap(),
            Some(SwapAccountRefusal::PreviousOrderLive)
        );
    }
    let expired = fixture.observe(31, 20).await;
    assert!(expired.swap().unwrap().admits_attempt());
    let visible = listed(&fixture.owner, fixture.operation)
        .expect("an account whose order ended is offered for another swap");
    assert_eq!(visible.last_pair(), Some((SELL, BUY)));
    // Hiding it keeps it out of the list; admission below still takes it.
    fixture.store.set_hidden(fixture.operation, true).unwrap();
    assert!(listed(&fixture.owner, fixture.operation).is_none());
    fixture.chain.lock().unwrap().rpc_methods.clear();
    let preview = fixture
        .owner
        .swap_order_preview(fixture.operation, true, None)
        .unwrap();
    assert!(preview.is_reused());
    assert_eq!(fixture.chain.lock().unwrap().rpc_methods, [] as [String; 0]);
    // The ended order isn't settled history, so admission reconciles it before judging.
    let selected = fixture
        .owner
        .reuse_swap_account(
            fixture.operation,
            30,
            SwapAccountRole::Source,
            SwapAccountUse::New,
            crate::OperationHttpClient::direct_for_tests(),
        )
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
        "checking setup and prior orders must share one account read"
    );
    // Another pair is another swap use of the account.
    fixture
        .store
        .claim_swap_pair(crate::vault::SwapPairClaim {
            id: crate::vault::SwapUseId::random().unwrap(),
            source: crate::vault::SwapAccountChoice::Existing(fixture.operation),
            delegate: fixture.delegate,
            purpose_summary: None,
            assets: Vec::new(),
            approval: SwapApproval {
                bounds: swap_bounds(),
                price_verified: None,
                price_acknowledged: false,
                delivery: SwapDelivery::Reshield,
                tokens: Some(SwapApprovalTokens {
                    sell: SELL,
                    buy: OTHER_BUY,
                }),
                accounts: None,
            },
            destination: None,
        })
        .unwrap();
    fixture.record_pair_attempt(1, 100, OTHER_BUY);
    let refused = fixture
        .owner
        .reuse_swap_account(
            fixture.operation,
            30,
            SwapAccountRole::Source,
            SwapAccountUse::New,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        refused.downcast_ref::<SwapAccountRefusal>(),
        Some(&SwapAccountRefusal::PreviousOrderLive),
        "{refused:#}"
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

/// The attempt `attempt` as a speed-up of the recorded order `open`: it shares that order's
/// use, pre-hook nonce, proof, delivery and sell amount, invalidates it, and asks for one unit
/// less of the buy token.
fn speed_up_attempt(
    fixture: &Fixture,
    attempt: u8,
    valid_to_block: u64,
    open: OrderUid,
) -> SwapAttempt {
    let record = fixture.record();
    let swap = record.swap().unwrap();
    let order = swap
        .orders()
        .iter()
        .find(|order| order.uid() == open)
        .unwrap();
    let bounds = SwapApprovedBounds {
        buy_amount: order.bounds().buy_amount - U256::ONE,
        ..order.bounds().clone()
    };
    SwapAttempt {
        invalidates: Some(open),
        speed_up: Some(open),
        ..fixture.attempt(
            attempt,
            valid_to_block,
            swap.order_terms(order).buy_token(),
            order.delivery(),
            bounds,
            order.bridge().cloned(),
        )
    }
}

/// Record order A as attempt 0 and its speed-up B as attempt 1, with Private delivery and
/// valid to the given blocks. Both pre-hooks are signed at the reconciled nonce 1 and share
/// the proof.
fn record_pair(fixture: &Fixture, valid_to: [u64; 2]) -> [OrderUid; 2] {
    fixture.record_attempt(0, valid_to[0]);
    let first = uid(0, valid_to[0]);
    fixture
        .store
        .record_swap_attempt(
            fixture.operation,
            speed_up_attempt(fixture, 1, valid_to[1], first),
        )
        .unwrap();
    [first, uid(1, valid_to[1])]
}

/// Record [`speed_up_attempt`] with its own bridge terms and the destination minimum they sign.
fn record_bridge_speed_up(
    fixture: &Fixture,
    attempt: u8,
    valid_to_block: u64,
    open: OrderUid,
    bridge: BridgeOrderTerms,
) {
    let speed_up = speed_up_attempt(fixture, attempt, valid_to_block, open);
    let bounds = SwapApprovedBounds {
        destination_minimum: Some(match &bridge {
            BridgeOrderTerms::Across(terms) => terms.output_amount,
            BridgeOrderTerms::NearIntents(terms) => terms.min_amount_out,
        }),
        ..speed_up.bounds.clone()
    };
    fixture
        .store
        .record_swap_attempt(
            fixture.operation,
            SwapAttempt {
                bounds,
                bridge: Some(bridge),
                ..speed_up
            },
        )
        .unwrap();
}

/// Put the shared proof's private effects in `block`, where the nonce passes the pre-hooks'
/// nonce 1 and the proof's notes read as spent. No order trades. Returns the transaction.
fn fund(fixture: &Fixture, block: u64) -> B256 {
    let transaction = B256::repeat_byte(0x30);
    let mut chain = fixture.chain.lock().unwrap();
    let logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
        .into_iter()
        .map(|data| (fixture.railgun, data))
        .collect();
    chain.add_logs(block, transaction, logs);
    chain.nonces.push((block, 2));
    chain.nullifiers_spent = Some(block);
    transaction
}

/// One settlement in `block` runs the shared proof, trades `order` and runs the post-hook of
/// attempt `shielder`, which credits `credit` and empties the executor. The nonce passes both
/// hooks.
fn settle_on_chain(fixture: &Fixture, block: u64, order: OrderUid, shielder: u8, credit: u64) {
    let mut chain = fixture.chain.lock().unwrap();
    let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
        .into_iter()
        .map(|data| (fixture.railgun, data))
        .collect::<Vec<_>>();
    logs.push((fixture.settlement, trade_log(order)));
    logs.push((fixture.railgun, post_hook_shield_log(shielder, credit)));
    chain.add_logs(block, B256::repeat_byte(0x40), logs);
    chain.nonces.push((block, 3));
    chain.nullifiers_spent = Some(block);
}

/// Record a broadcaster-funded cancellation at the pre-hooks' nonce 1 that runs `calls` and
/// pays its fee with `fee`. Returns the payload's hash.
fn record_cancellation(fixture: &Fixture, calls: Vec<Call>, fee: &Utxo) -> B256 {
    let hash = B256::repeat_byte(0x39);
    fixture
        .store
        .record_issued(
            fixture.operation,
            IssuedExecutorPayload::new(
                U256::ONE,
                fixture.delegate,
                hash,
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(
                    execute(vec![private_transaction(0x31, 0x32)], calls, 1),
                    fixture.record().nonce_observation().unwrap(),
                    vec![ExecutorInputIdentity::from_utxo(fee)],
                ),
            ),
        )
        .unwrap();
    hash
}

/// The fixture's record as a reopened store reads it.
fn stored(fixture: &Fixture) -> ExecutorRecord {
    ExecutorStore::new(fixture.db.clone(), fixture.view.clone(), 1)
        .unwrap()
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == fixture.operation)
        .unwrap()
}

/// Every order's state, in the record's order.
fn order_states(record: &ExecutorRecord) -> Vec<SwapOrderState> {
    let swap = record.swap().unwrap();
    swap.orders()
        .iter()
        .map(|order| swap_order_state(swap, order))
        .collect()
}

#[tokio::test]
async fn only_a_speed_up_is_admitted_while_the_first_order_is_still_live() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    // The pre-hook's nonce is unused, nothing is filled, and `validTo` is ahead.
    let open = fixture.observe(31, 20).await;
    assert_eq!(state(&open, 0), SwapOrderState::Open);
    let swap_use = SwapUseId::first(fixture.operation);
    assert_eq!(open.active_swap_use(), Some(swap_use));
    let first = uid(0, 100);
    let binding = SwapSpeedUp {
        swap_use,
        order: first,
        pre_hook_nonce: open.swap().unwrap().orders()[0].pre_hook().nonce(),
    };

    // The swap's own use is refused the account as a retry while that order can still execute.
    let refused = fixture
        .owner
        .reuse_swap_account(
            fixture.operation,
            30,
            SwapAccountRole::Source,
            SwapAccountUse::Claimed(swap_use),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        refused.downcast_ref::<SwapAccountRefusal>(),
        Some(&SwapAccountRefusal::PreviousOrderLive),
        "{refused:#}"
    );
    // A speed-up that names the live order is admitted at that order's nonce.
    let admitted = fixture
        .owner
        .reuse_swap_account(
            fixture.operation,
            30,
            SwapAccountRole::Source,
            SwapAccountUse::SpeedUp(binding),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(admitted.expected_pre_hook_nonce(), binding.pre_hook_nonce);

    // The replacement shares the use, the pre-hook's nonce and the proof, sells the same
    // amount and asks for less.
    let live = fixture.record();
    let swap = live.swap().unwrap();
    let first_order = &swap.orders()[0];
    let speed_up = speed_up_attempt(&fixture, 1, 110, first);
    assert_eq!(speed_up.use_id, swap_use);
    assert_eq!(speed_up.pre_hook.nonce(), first_order.pre_hook().nonce());
    assert_eq!(&speed_up.proof, swap.proof());
    assert_eq!(
        speed_up.bounds.sell_amount,
        first_order.bounds().sell_amount
    );
    assert!(speed_up.bounds.buy_amount < first_order.bounds().buy_amount);
    // Recorded as an ordinary retry it is refused, since the live order isn't ended.
    assert!(matches!(
        fixture.store.record_swap_attempt(
            fixture.operation,
            SwapAttempt {
                speed_up: None,
                ..speed_up.clone()
            },
        ),
        Err(ExecutorStoreError::SwapAttemptOutstanding)
    ));
    assert_eq!(fixture.record(), live);
    // A speed-up is refused when it asks for the open order's buy amount, funds from another
    // proof, or doesn't invalidate the open order.
    for refused in [
        SwapAttempt {
            bounds: first_order.bounds().clone(),
            ..speed_up.clone()
        },
        SwapAttempt {
            proof: SwapProof::new(B256::repeat_byte(0x21), swap.proof().inputs().to_vec()),
            ..speed_up.clone()
        },
        SwapAttempt {
            invalidates: None,
            ..speed_up.clone()
        },
    ] {
        assert!(matches!(
            fixture
                .store
                .record_swap_attempt(fixture.operation, refused),
            Err(ExecutorStoreError::SwapSpeedUpRefused)
        ));
        assert_eq!(fixture.record(), live);
    }

    // The speed-up is recorded beside the live order: one use, one pre-hook nonce, one proof.
    fixture
        .store
        .record_swap_attempt(fixture.operation, speed_up.clone())
        .unwrap();
    let record = fixture.record();
    let [kept, added] = record.swap().unwrap().orders() else {
        panic!("the speed-up is the second order");
    };
    assert_eq!((kept.uid(), added.uid()), (first, speed_up.uid));
    assert_eq!(added.use_id(), kept.use_id());
    assert_eq!(added.pre_hook().nonce(), kept.pre_hook().nonce());
    assert_eq!(added.invalidates(), Some(first));
    assert_eq!(record.swap().unwrap().proof(), swap.proof());
    // The live order and its use keep what was approved for them, and the new order has its
    // own bounds.
    assert_eq!(kept, first_order);
    assert_eq!(record.swap_uses(), live.swap_uses());
    assert_eq!(added.bounds(), &speed_up.bounds);
    // Both orders survive reopening the store.
    let reopened = ExecutorStore::new(fixture.db.clone(), fixture.view.clone(), 1)
        .unwrap()
        .records()
        .unwrap()
        .into_iter()
        .find(|reopened| reopened.operation() == fixture.operation)
        .unwrap();
    assert_eq!(reopened, record);

    // The earlier order is no longer the latest, so another speed-up can't name it.
    assert!(matches!(
        fixture
            .store
            .record_swap_attempt(fixture.operation, speed_up_attempt(&fixture, 2, 120, first)),
        Err(ExecutorStoreError::SwapSpeedUpRefused)
    ));
    assert_eq!(fixture.record(), record);
    fixture.finish().await;
}

// The account rule and the store admit a speed-up for an order paid from the private balance
// unless it delivers privately through a bridge, and only for the binding of the latest order
// while the account's nonce is still the one both pre-hooks share.
#[tokio::test]
async fn speed_up_admission_follows_the_delivery_the_binding_and_the_shared_nonce() {
    let receiver = Address::repeat_byte(0x77);
    for scenario in 0..5 {
        let fixture = Fixture::start().await;
        let spoke_pool = fixture.config.bridge_profile().unwrap().spoke_pool();
        // 0: Private delivery. 1: a Public address. 2: Across to a public receiver. 3: NEAR
        // Intents to a public receiver. 4: Across to the wallet's private balance.
        let (delivery, bridge, supported) = match scenario {
            0 => (SwapDelivery::Reshield, None, true),
            1 => (SwapDelivery::External { receiver }, None, true),
            2 => {
                let (delivery, terms) = across_order(spoke_pool);
                (
                    SwapDelivery::Bridge(delivery),
                    Some(BridgeOrderTerms::Across(terms)),
                    true,
                )
            }
            3 => (
                SwapDelivery::Bridge(BridgeDelivery {
                    provider: BridgeProvider::NearIntents,
                    destination_chain: 56,
                    receiver,
                    destination_token: Address::ZERO,
                    surplus: BridgeSurplus::BridgedByProvider,
                    private: None,
                }),
                Some(BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
                    deposit_address: Address::repeat_byte(0x79),
                    min_amount_out: U256::from(15),
                    amount_out: U256::from(16),
                    deadline: "2026-09-30T01:00:00.000Z".into(),
                    signed_quote: "{}".into(),
                })),
                true,
            ),
            _ => {
                let (delivery, terms) =
                    private_across_order(spoke_pool, Address::repeat_byte(0x7a));
                (
                    SwapDelivery::Bridge(delivery),
                    Some(BridgeOrderTerms::Across(terms)),
                    false,
                )
            }
        };
        fixture.record_delivery_attempt(1, 100, BUY, delivery.clone(), bridge);
        let record = fixture.record();
        let open = &record.swap().unwrap().orders()[0];
        let binding = |order: &SwapOrderRecord| SwapSpeedUp {
            swap_use: order.use_id().unwrap(),
            order: order.uid(),
            pre_hook_nonce: order.pre_hook().nonce(),
        };
        let refusal = |record: &ExecutorRecord, speed_up| {
            swap_account_refusal(
                record,
                1,
                SwapAccountRole::Source,
                SwapAccountUse::SpeedUp(speed_up),
                SwapAdmissionEvidence::Fresh,
            )
        };
        let unsupported = (!supported).then_some(SwapAccountRefusal::SpeedUpUnsupported);
        assert_eq!(
            refusal(&record, binding(open)),
            unsupported,
            "scenario {scenario}"
        );
        // A binding to another order, or to another nonce than the open order's pre-hook, is
        // not this swap's open order.
        for changed in [
            SwapSpeedUp {
                order: uid(9, 100),
                ..binding(open)
            },
            SwapSpeedUp {
                pre_hook_nonce: open.pre_hook().nonce() + U256::ONE,
                ..binding(open)
            },
        ] {
            assert_eq!(
                refusal(&record, changed),
                Some(SwapAccountRefusal::SpeedUpOrderChanged),
                "scenario {scenario}"
            );
        }

        let stored = fixture.store.record_swap_attempt(
            fixture.operation,
            speed_up_attempt(&fixture, 2, 110, open.uid()),
        );
        if !supported {
            assert!(
                matches!(stored, Err(ExecutorStoreError::SwapSpeedUpRefused)),
                "scenario {scenario}"
            );
            assert_eq!(fixture.record(), record);
            fixture.finish().await;
            continue;
        }
        let stored = stored.unwrap();
        let latest = stored.swap().unwrap().orders().last().unwrap();
        assert_eq!(latest.delivery(), delivery, "scenario {scenario}");
        assert_eq!(
            refusal(&stored, binding(latest)),
            None,
            "scenario {scenario}"
        );

        // The account's nonce moves past the shared one with no pre-hook attributed. Either
        // order's pre-hook may have taken it, so nothing is signed at it again.
        let observed = stored.nonce_observation().unwrap();
        let moved = fixture
            .store
            .record_account_read(
                fixture.operation,
                ExecutorNonceObservation::new(
                    BlockNumHash::new(observed.block().number + 1, B256::repeat_byte(0xab)),
                    observed.nonce() + U256::ONE,
                ),
            )
            .unwrap();
        assert!(
            moved
                .swap()
                .unwrap()
                .orders()
                .iter()
                .all(|order| order.observations().pre_hook_executed.is_none())
        );
        assert_eq!(
            refusal(&moved, binding(latest)),
            Some(SwapAccountRefusal::SpeedUpNonceUsed),
            "scenario {scenario}"
        );
        fixture.finish().await;
    }
}

// A pre-hook invalidates one order, so a speed-up waits while two orders can still fill. An
// order without a recorded expiry counts until local time is past its `validTo` by the
// profile's whole validity window.
#[tokio::test]
async fn a_speed_up_waits_until_its_open_order_is_the_only_one_that_can_fill() {
    let fixture = Fixture::start().await;
    let profile = swap_profile();
    // The second order ends more than a validity window after the first.
    let (first, second) = (uid(0, 100), uid(1, 160));
    let (first_ends, second_ends) = (at_block(100), at_block(160));
    assert!(first_ends + profile.valid_to_window() < second_ends);
    let judged =
        |order, now| crate::speed_up_block(&fixture.record(), &profile, 1, order, now, None);
    let binding = |order| SwapSpeedUp {
        swap_use: SwapUseId::first(fixture.operation),
        order,
        pre_hook_nonce: U256::ONE,
    };

    // One open order can be sped up.
    fixture.record_attempt(0, 100);
    assert_eq!(judged(first, at_block(20)), Ok(binding(first)));
    fixture
        .store
        .record_swap_attempt(fixture.operation, speed_up_attempt(&fixture, 1, 160, first))
        .unwrap();
    assert_eq!(
        judged(second, at_block(20)),
        Err(SpeedUpBlock::TwoOrdersFillable)
    );

    // Local time at the first order's `validTo` plus the window still counts it. One second
    // later it drops out, and the second order is the open one.
    let boundary = first_ends + profile.valid_to_window();
    assert_eq!(
        judged(second, boundary),
        Err(SpeedUpBlock::TwoOrdersFillable)
    );
    assert_eq!(
        judged(second, boundary + Duration::from_secs(1)),
        Ok(binding(second))
    );
    // An order at its own `validTo` is not sped up.
    assert_eq!(judged(second, second_ends), Err(SpeedUpBlock::Expired));

    // A recorded finalized expiry of the first order admits the second at once, while both
    // are still unexpired by local time.
    let seen = SwapObservation {
        block: fixture.record().nonce_observation().unwrap().block(),
        transaction_hash: None,
    };
    for expiry in [
        SwapOrderObservations {
            pre_hook_dead: Some(SwapPreHookDeath {
                cause: SwapPreHookDeathCause::Expired,
                observation: seen,
            }),
            ..SwapOrderObservations::default()
        },
        SwapOrderObservations {
            expired: Some(seen),
            ..SwapOrderObservations::default()
        },
    ] {
        fixture
            .store
            .record_swap_observations(fixture.operation, first, expiry)
            .unwrap();
        assert_eq!(judged(second, at_block(20)), Ok(binding(second)));
    }
    fixture.finish().await;
}

// A pending setup's pass reads the account at the latest block too, on the same endpoint and
// about the same account, and returns what it shows. The record keeps the confirmed read.
#[tokio::test]
async fn a_pending_setup_pass_reads_the_latest_block_once_and_records_nothing() {
    let fixture = Fixture::start_pending().await;
    let other_code = Bytes::from(
        [
            EIP7702_DELEGATION_DESIGNATOR.as_slice(),
            Address::repeat_byte(0xdd).as_slice(),
        ]
        .concat(),
    );
    let latest_reads = |requests: &[Value]| {
        requests
            .iter()
            .filter(|request| {
                request["method"] == "eth_getBlockByNumber" && request["params"][0] == "latest"
            })
            .count()
    };
    // The confirmed block 14 shows the setup's nonce 0 unconsumed. The latest block 15 shows
    // the delegation and nonce 1, the nonce unconsumed, or another delegation.
    for (nonce, code, included) in [
        (1, None, true),
        (0, None, false),
        (1, Some(other_code), false),
    ] {
        let before = {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 15;
            chain.nonces = vec![(15, nonce)];
            chain.head_code = code;
            chain.rpc_requests.len()
        };
        let observed = fixture
            .owner
            .observe_swap_setup(
                fixture.operation,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(observed.status, crate::SwapSetupStatus::Pending);
        assert_eq!(
            observed
                .inclusion
                .map(|inclusion| (inclusion.block(), inclusion.nonce())),
            included.then_some((15, U256::ONE))
        );
        let chain = fixture.chain.lock().unwrap();
        let pass = &chain.rpc_requests[before..];
        assert_eq!(latest_reads(pass), 1);
        assert!(
            pass.iter()
                .filter(|request| request["method"] == "eth_getCode")
                .all(|request| request["params"][0] == json!(EXECUTOR))
        );
        let at_confirmed = ExecutorNonceObservation::new(chain.block(14), U256::ZERO);
        drop(chain);
        let record = fixture.record();
        assert_eq!(record.nonce_observation(), Some(at_confirmed));
        assert_eq!(
            record.payload_state(fixture.setup),
            Some(ExecutorPayloadState::Pending)
        );
    }

    // A confirmed read resolves the setup, and the pass reads no latest block.
    let before = {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 16;
        chain.nonces = vec![(14, 1)];
        chain.head_code = None;
        chain.rpc_requests.len()
    };
    let observed = fixture
        .owner
        .observe_swap_setup(
            fixture.operation,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert!(matches!(
        observed.status,
        crate::SwapSetupStatus::Delegated(_)
    ));
    assert_eq!(observed.inclusion, None);
    assert_eq!(
        latest_reads(&fixture.chain.lock().unwrap().rpc_requests[before..]),
        0
    );
    fixture.finish().await;
}

// An order on an included setup names the record's confirmed observation, which stays the floor
// of its status reads. A pre-hook that ran outside a trade in a block below the latest block
// read at signing is still found.
#[tokio::test]
async fn an_order_on_an_included_setup_is_observed_from_the_confirmed_block() {
    let fixture = Fixture::start_pending().await;
    let receiver = Address::repeat_byte(0x77);
    let delivery = SwapDelivery::External { receiver };
    let attempt = fixture.attempt_after(
        0,
        100,
        BUY,
        delivery,
        SwapApprovedBounds {
            post_hook_gas_limit: None,
            ..swap_bounds()
        },
        None,
        1,
    );
    // Signed while the latest block 15 showed the setup included.
    let record = fixture
        .store
        .record_swap_attempt_on_included_setup(fixture.operation, attempt, U256::ONE)
        .unwrap();
    let floor = record
        .issued()
        .iter()
        .find(|payload| payload.purpose() == ExecutorPayloadPurpose::SwapPreHook)
        .unwrap()
        .context()
        .observed()
        .block()
        .number;
    assert_eq!(floor, 13);
    // The setup and the pre-hook run in block 14.
    fund(&fixture, 14);
    let record = fixture.observe(17, floor).await;
    assert!(matches!(
        state(&record, 0),
        SwapOrderState::PreHookOnly { .. }
    ));
    assert_eq!(
        record.payload_state(fixture.setup),
        Some(ExecutorPayloadState::Resolved)
    );
    fixture.finish().await;
}

#[tokio::test]
async fn an_approved_or_restricted_setup_is_not_offered_for_another_swap() {
    let fixture = Fixture::start().await;
    let refusal = |operation| {
        fixture
            .owner
            .swap_account_refusal(operation, SwapAccountRole::Source)
            .unwrap()
    };
    // A setup whose approved order still awaits placement belongs to that swap.
    assert!(listed(&fixture.owner, fixture.operation).is_none());
    assert_eq!(
        refusal(fixture.operation),
        Some(SwapAccountRefusal::ClaimedByAnotherSwap)
    );
    // Each restriction keeps an account out of the list for its own reason: a delegate this
    // chain doesn't accept, an account kept for recovery, and one registered in Public.
    let [foreign, public] = std::array::from_fn(|_| ExecutorOperationId::random().unwrap());
    for (operation, delegate, address) in [
        (foreign, Address::repeat_byte(7), Address::repeat_byte(0x71)),
        (public, fixture.delegate, Address::repeat_byte(0x72)),
    ] {
        fixture
            .store
            .reserve(operation, delegate, Some("Private swap"), &[])
            .unwrap();
        fixture.store.bind_address(operation, address).unwrap();
    }
    fixture
        .store
        .register_public_account(public, Address::repeat_byte(0x72))
        .unwrap();
    let recovered = fixture
        .store
        .restore_index(40, Address::repeat_byte(0x73), fixture.delegate, &[])
        .unwrap()
        .operation();
    assert_eq!(
        [foreign, public, recovered].map(refusal),
        [
            Some(SwapAccountRefusal::UnsupportedDelegate),
            Some(SwapAccountRefusal::RegisteredInPublic),
            Some(SwapAccountRefusal::RecoveryOnly),
        ]
    );
    assert_eq!(
        fixture.owner.swap_account_candidates().unwrap(),
        [] as [crate::SwapAccountCandidate; 0]
    );
    fixture.finish().await;
}

// A source preparation cancelled after its setup was sent keeps its account until a confirmed
// read resolves the setup and every payload the account signed. Only then is the account
// offered to a new swap as a source, and fresh checks still read its delegation.
#[tokio::test]
async fn a_cancelled_source_is_offered_again_once_its_setup_and_work_resolve() {
    let fixture = Fixture::start_pending().await;
    let first = SwapUseId::first(fixture.operation);
    fixture
        .owner
        .record_swap_approval(
            fixture.operation,
            first,
            SwapApproval {
                bounds: swap_bounds(),
                price_verified: Some(false),
                price_acknowledged: true,
                delivery: SwapDelivery::Reshield,
                tokens: None,
                accounts: None,
            },
        )
        .unwrap();
    assert_eq!(
        fixture
            .store
            .cancel_swap_use(fixture.operation, first)
            .unwrap()
            .source,
        crate::vault::SwapUseRelease::IssuedWorkRemains
    );
    let refusal = |role| {
        fixture
            .owner
            .swap_account_refusal(fixture.operation, role)
            .unwrap()
    };
    // The setup is only in the head block, above the confirmed one.
    assert_eq!(
        refusal(SwapAccountRole::Source),
        Some(SwapAccountRefusal::SetupUnconfirmed)
    );
    assert!(listed(&fixture.owner, fixture.operation).is_none());

    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 16;
        chain.nonces = vec![(14, 1)];
    }
    fixture
        .owner
        .reconcile_account(
            fixture.operation,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(refusal(SwapAccountRole::Source), None);
    assert!(listed(&fixture.owner, fixture.operation).is_some());
    // The cancelled use itself never continues, and the account takes no destination use.
    assert_eq!(
        swap_account_refusal(
            &fixture.record(),
            1,
            SwapAccountRole::Source,
            SwapAccountUse::Claimed(first),
            SwapAdmissionEvidence::Recorded,
        ),
        Some(SwapAccountRefusal::RecoveryOnly)
    );
    assert_eq!(
        refusal(SwapAccountRole::Destination { token: BUY }),
        Some(SwapAccountRefusal::RecoveryOnly)
    );

    // Fresh checks refuse another delegation at the confirmed block and sign nothing.
    let issued = fixture.record().issued().to_vec();
    fixture.chain.lock().unwrap().delegate = Address::repeat_byte(0xdd);
    assert_eq!(
        reuse_refusal(&fixture.owner, fixture.operation, 15).await,
        SwapAccountRefusal::SetupUnconfirmed
    );
    fixture.chain.lock().unwrap().delegate = fixture.delegate;
    let reused = fixture
        .owner
        .reuse_swap_account(
            fixture.operation,
            15,
            SwapAccountRole::Source,
            SwapAccountUse::New,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert!(reused.is_reused());
    assert_eq!(fixture.record().issued(), issued.as_slice());

    // A recovery signed after the cancellation retires the account for good.
    record_cancellation(&fixture, Vec::new(), &sell_note(5));
    assert_eq!(
        refusal(SwapAccountRole::Source),
        Some(SwapAccountRefusal::RecoveryOnly)
    );
    assert_eq!(
        reuse_refusal(&fixture.owner, fixture.operation, 15).await,
        SwapAccountRefusal::RecoveryOnly
    );
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

// A completed order must not hide a sibling of its swap use that still awaits its post-hook
// shield: the page holding that shield is read even after every hook nonce of the use passed.
#[tokio::test]
async fn a_later_shield_completes_a_second_trade_of_the_same_swap_use() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    // One settlement runs the first attempt's pre-hook, trades, and runs its post-hook.
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        logs.push((fixture.settlement, trade_log(uid(0, 100))));
        logs.push((fixture.railgun, post_hook_shield_log(0, 9_975)));
        chain.add_logs(40, B256::repeat_byte(0x40), logs);
        chain.nonces.push((40, 3));
    }
    let record = fixture.observe(46, 35).await;
    assert_eq!(state(&record, 0), SwapOrderState::Done);

    // A second attempt of the same use signs its hooks at the next nonces, 3 and 4.
    fixture.record_attempt(1, 200);
    let record = fixture.record();
    let orders = record.swap().unwrap().orders();
    assert!(orders[1].use_id().is_some() && orders[1].use_id() == orders[0].use_id());
    // Its settlement runs the pre-hook and trades, but the post-hook does not run: the
    // nonce stops at the post-hook's and the proceeds stay in the executor.
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        logs.push((fixture.settlement, trade_log(uid(1, 200))));
        chain.add_logs(50, B256::repeat_byte(0x50), logs);
        chain.nonces.push((50, 4));
        chain.buy_balance.push((50, BUY_AMOUNT));
    }
    let record = fixture.observe(56, 46).await;
    assert_eq!(state(&record, 1), SwapOrderState::NotDelivered);

    // The post-hook runs later on its own. Every hook nonce of the use is now consumed.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.add_logs(
            60,
            B256::repeat_byte(0x60),
            vec![(fixture.railgun, post_hook_shield_log(1, 9_975))],
        );
        chain.nonces.push((60, 5));
        chain.buy_balance.push((60, 1));
    }
    let record = fixture.observe(66, 56).await;
    assert_eq!(state(&record, 1), SwapOrderState::Done);
    assert_eq!(state(&record, 0), SwapOrderState::Done);
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
        .reconcile_account(
            fixture.operation,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
        .record()
        .clone();
    let closed = fixture
        .owner
        .swap_orders_closed(
            &fixture.config,
            &record,
            crate::OperationHttpClient::direct_for_tests(),
        )
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
            .swap_orders_closed(
                &fixture.config,
                &filled,
                crate::OperationHttpClient::direct_for_tests()
            )
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
            .swap_orders_closed(
                &fixture.config,
                &expired,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err()
    );
    fixture.finish().await;
}

/// A stealth account's confirmed read, its swap's order observation and a signing inspection
/// go only over the account's operation client, never over the session's shared RPC client.
#[tokio::test]
async fn stealth_account_reads_use_only_the_account_client() {
    use crate::rpc_broker::tests::{spawn_recording_rpc_mock, tagged_reqwest_client};
    let shared = || {
        HttpContext::with_rpc_client_for_tests(
            tagged_reqwest_client("shared"),
            crate::WalletNetworkMode::Direct,
        )
    };
    let account = crate::OperationHttpClient::for_tests(
        tagged_reqwest_client("account"),
        crate::OperationNetworkIsolation::Dedicated,
    );
    let only_account = |routes: &[Option<String>]| {
        assert_ne!(routes, [] as [Option<String>; 0]);
        assert!(
            routes
                .iter()
                .all(|route| route.as_deref() == Some("account")),
            "{routes:?}"
        );
    };

    // The swap's account: its confirmed read, then its order observation.
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    fixture.chain.lock().unwrap().head = 31;
    let served = fixture.chain.clone();
    let (endpoint, recorded, server) = spawn_recording_rpc_mock(Arc::new(move |request: Value| {
        served.lock().unwrap().respond(&request)
    }))
    .await;
    let mut config = fixture.config.clone();
    config.rpc_route = crate::RpcChainRoute::new(1, vec![endpoint]);
    let owner = ExecutorOwner::new(
        1,
        fixture.db.clone(),
        fixture.view.clone(),
        config,
        shared(),
    )
    .unwrap();
    owner
        .reconcile_account(fixture.operation, &account)
        .await
        .unwrap();
    owner
        .observe_swap(fixture.operation, 20..31, &account)
        .await
        .unwrap();
    let routes: Vec<_> = recorded
        .lock()
        .unwrap()
        .iter()
        .map(|(route, _)| route.clone())
        .collect();
    only_account(&routes);
    server.abort();
    owner.shutdown().await;
    drop(owner);
    fixture.finish().await;

    // A fresh account's signing inspection while it is prepared.
    let rpc = super::Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let owner =
        ExecutorOwner::new(0, db.clone(), view.clone(), super::chain(&rpc), shared()).unwrap();
    owner
        .prepare_operation(
            ExecutorOperationId::random().unwrap(),
            super::delivery(),
            &super::swap_setup::password(),
            &[],
            None,
            &account,
        )
        .await
        .unwrap();
    only_account(&rpc.state.routes.lock().unwrap());
    owner.shutdown().await;
    drop(owner);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn expired_pre_hook_releases_inputs_at_finality_and_stays_released_after_restart() {
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
    assert_eq!(
        restarted.input_locks(&[sell_note(3)]).unwrap(),
        [] as [crate::ExecutorInputLock; 0]
    );
    assert_eq!(fixture.chain.lock().unwrap().rpc_methods.len(), requests);
    restarted.shutdown().await;
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
        .check_input_lock(
            fixture.operation,
            30,
            &notes,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert!(lock.is_none());
    assert_eq!(
        state(&fixture.record(), 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    assert_eq!(
        fixture.owner.input_locks(&notes).unwrap(),
        [] as [crate::ExecutorInputLock; 0]
    );
    fixture.finish().await;
}

#[tokio::test]
async fn invalidated_order_names_a_cancellation_without_a_reported_transaction_or_sync() {
    for previously_unknown in [false, true] {
        let fixture = Fixture::start().await;
        fixture.record_attempt(0, 200);
        let order = uid(0, 200);
        let observed = fixture.record().nonce_observation().unwrap();
        fixture
            .store
            .record_issued(
                fixture.operation,
                IssuedExecutorPayload::new(
                    U256::ONE,
                    fixture.delegate,
                    B256::repeat_byte(0x39),
                    ExecutorPayloadPurpose::Recovery,
                    ExecutorPayloadContext::new(
                        execute(
                            vec![private_transaction(0x31, 0x32)],
                            vec![invalidation(fixture.settlement, order)],
                            1,
                        ),
                        observed,
                        vec![ExecutorInputIdentity::from_utxo(&sell_note(4))],
                    ),
                ),
            )
            .unwrap();
        fixture.chain.lock().unwrap().nonces.push((30, 2));
        if previously_unknown {
            let record = fixture.observe(36, 25).await;
            assert_eq!(
                state(&record, 0),
                SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Unknown)
            );
            // The unresolved post-hook makes the next page worth reading, so page selection
            // skips the filled-amount probe. The Unknown cause still needs that order state.
            fixture.chain.lock().unwrap().nonces.push((31, 3));
        }
        fixture.chain.lock().unwrap().invalidated.push((order, 30));
        let record = fixture.observe(37, 25).await;
        assert_eq!(
            state(&record, 0),
            SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)
        );
        assert_eq!(
            crate::swap_invalidation(&record, &swap_profile(), at_block(37)).unwrap(),
            None
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn cancellation_and_an_older_post_hook_end_attempts_but_a_copied_shield_does_not() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 200);
    let first = uid(0, 200);

    // A broadcaster-funded cancellation at the pre-hook's nonce invalidates the order. The
    // broadcaster reported its transaction, which pays the fee with a note of its own.
    let observed = fixture.record().nonce_observation().unwrap();
    let cancellation = B256::repeat_byte(0x39);
    let sent = B256::repeat_byte(0x3b);
    let fee = sell_note(4);
    fixture
        .store
        .record_issued(
            fixture.operation,
            IssuedExecutorPayload::new(
                U256::ONE,
                fixture.delegate,
                cancellation,
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(
                    execute(
                        vec![private_transaction(0x31, 0x32)],
                        vec![invalidation(fixture.settlement, first)],
                        1,
                    ),
                    observed,
                    vec![ExecutorInputIdentity::from_utxo(&fee)],
                ),
            ),
        )
        .unwrap();
    fixture
        .store
        .record_submission(fixture.operation, cancellation, sent)
        .unwrap();
    // No read finds the transaction: only the nonce shows that something ran.
    fixture.chain.lock().unwrap().nonces.push((30, 2));
    let record = fixture.observe(36, 25).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Unknown)
    );
    // Until the cause is known the order counts as fillable, so a retry invalidates it again.
    assert_eq!(
        crate::swap_invalidation(&record, &swap_profile(), at_block(36)).unwrap(),
        Some(first)
    );
    // Private sync shows the fee note spent in the recorded transaction.
    let mut spent = railgun_wallet::WalletUtxo::new(fee);
    spent.spent = Some(UtxoSource {
        tx_hash: sent,
        block_number: 30,
        block_timestamp: timestamp(30),
    });
    let record = fixture.observe_synced(36, 25, synced(vec![spent])).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)
    );
    // The pre-hook's nonce is consumed, so its notes wait for private sync to pass that read.
    assert!(fixture.reserved(&record));
    let resolved_at = record.nonce_watermark().unwrap().block();
    let record = fixture
        .store
        .record_synced(fixture.operation, None, resolved_at)
        .unwrap();
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
    // No direct call to the executor won nonce k + 1, but the nonce read past it resolves it,
    // so the next retry is admitted at k + 2.
    fixture.record_attempt(2, 400);
    let record = fixture.record();
    assert_eq!(
        record.swap().unwrap().orders()[2].pre_hook().nonce(),
        U256::from(3)
    );
    fixture.finish().await;
}

#[tokio::test]
async fn a_received_shield_names_the_recovery_over_a_cancellation_at_the_pre_hooks_nonce() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 200);
    let first = uid(0, 200);
    let observed = fixture.record().nonce_observation().unwrap();
    // A cancellation and a recovery that also shields the sell token, both at the pre-hook's
    // nonce and neither with a recorded transaction.
    let request = ShieldRequest {
        preimage: CommitmentPreimage {
            npk: B256::repeat_byte(0x70),
            token: TokenData::erc20(SELL),
            value: U120::from(SELL_AMOUNT),
        },
        ..post_hook_shield(0)
    };
    let shield = Call {
        to: EXECUTOR,
        value: U256::ZERO,
        data: shieldCall {
            _shieldRequests: vec![request.clone()],
        }
        .abi_encode()
        .into(),
    };
    let invalidate = invalidation(fixture.settlement, first);
    for (hash, calls) in [
        (0x39, vec![invalidate.clone()]),
        (0x3a, vec![invalidate, shield]),
    ] {
        fixture
            .store
            .record_issued(
                fixture.operation,
                IssuedExecutorPayload::new(
                    U256::ONE,
                    fixture.delegate,
                    B256::repeat_byte(hash),
                    ExecutorPayloadPurpose::Recovery,
                    ExecutorPayloadContext::new(
                        execute(vec![private_transaction(0x31, 0x32)], calls, 1),
                        observed,
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
    }
    fixture.chain.lock().unwrap().nonces.push((30, 2));
    // Nothing tells the two apart, and the cancellation is not named for lack of a shield.
    let record = fixture.observe(36, 25).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Unknown)
    );

    let received = railgun_wallet::WalletUtxo::new(Utxo::new(
        request.preimage.note_with_random([0; 16]),
        0,
        50,
        UtxoSource {
            tx_hash: B256::repeat_byte(0x3c),
            block_number: 30,
            block_timestamp: timestamp(30),
        },
        UtxoCommitmentKind::Shield,
    ));
    let record = fixture.observe_synced(36, 25, synced(vec![received])).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Recovery)
    );
    fixture.finish().await;
}

/// `invalidateOrder(uid)` on the settlement.
fn invalidation(settlement: Address, uid: OrderUid) -> Call {
    Call {
        to: settlement,
        value: U256::ZERO,
        data: GPv2Settlement::invalidateOrderCall {
            orderUid: uid.0.to_vec().into(),
        }
        .abi_encode()
        .into(),
    }
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
    assert!(
        record.reserved_inputs().contains(&fee),
        "the loser's fee note waits for private sync like every note at the consumed nonce"
    );
    let resolved_at = record.nonce_watermark().unwrap().block();
    let record = fixture
        .store
        .record_synced(fixture.operation, None, resolved_at)
        .unwrap();
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
    };
    // The broadcaster fee is paid in the swap's sell token.
    let fee = MixedPrivateActionRequest {
        adapter: None,
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

// Both orders' pre-hooks share the proof and the nonce. The proof's effects in the block where
// the nonce passes show that the account was funded, and not which pre-hook ran.
#[tokio::test]
async fn shared_proof_effects_record_one_funding_and_name_neither_pre_hook() {
    let fixture = Fixture::start().await;
    let [_, second] = record_pair(&fixture, [100, 110]);
    let (swap_use, k) = (SwapUseId::first(fixture.operation), U256::ONE);
    let transaction = fund(&fixture, 30);
    let record = fixture.observe(36, 25).await;
    let funding = SwapObservation {
        block: fixture.chain.lock().unwrap().block(30),
        transaction_hash: Some(transaction),
    };
    let swap = record.swap().unwrap();
    let [funded] = swap.fundings() else {
        panic!("one funding for the use and the shared nonce");
    };
    assert_eq!(
        (funded.use_id, funded.pre_hook_nonce, funded.observation),
        (swap_use, k, funding)
    );
    for order in swap.orders() {
        let observed = order.observations();
        assert_eq!(
            (
                observed.pre_hook_executed,
                observed.pre_hook_dead,
                observed.traded
            ),
            (None, None, None)
        );
        assert_eq!(swap.funding(order), Some(funding));
        assert_eq!(
            swap_order_state(swap, order),
            SwapOrderState::PreHookOnly { expired: false }
        );
    }
    assert_eq!(
        (record.swap_hook_winner(k), record.resolved_swap_hook(k)),
        (None, None)
    );

    // Whichever pre-hook ran took the shared nonce, so nothing more is signed at it, and a
    // retry waits as for an executed pre-hook.
    let refusal = |swap_use| {
        swap_account_refusal(
            &record,
            1,
            SwapAccountRole::Source,
            swap_use,
            SwapAdmissionEvidence::Fresh,
        )
    };
    assert_eq!(
        refusal(SwapAccountUse::SpeedUp(SwapSpeedUp {
            swap_use,
            order: second,
            pre_hook_nonce: k,
        })),
        Some(SwapAccountRefusal::SpeedUpNonceUsed)
    );
    assert_eq!(
        refusal(SwapAccountUse::Claimed(swap_use)),
        Some(SwapAccountRefusal::PreviousOrderLive)
    );
    assert!(matches!(
        fixture.store.record_swap_attempt(
            fixture.operation,
            fixture.attempt(2, 120, BUY, SwapDelivery::Reshield, swap_bounds(), None),
        ),
        Err(ExecutorStoreError::SwapAttemptOutstanding)
    ));
    assert!(
        fixture.reserved(&record),
        "the spent notes wait for private sync"
    );

    // The funded pair has nothing left to find, so a later page's logs aren't read, and a
    // reopened store holds the same funding.
    let log_queries = || fixture.chain.lock().unwrap().log_queries;
    let before = log_queries();
    let later = fixture.observe(37, 36).await;
    assert_eq!(log_queries(), before);
    assert_eq!(later.swap(), record.swap());
    assert_eq!(stored(&fixture), later);

    // A reorg of the funding's block removes it, and both orders are open again.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.reorg(29);
        chain.nonces = vec![(11, 1)];
        chain.nullifiers_spent = None;
    }
    let reopened = fixture.observe(38, 30).await;
    assert_eq!(reopened.swap().unwrap().fundings(), [] as [SwapFunding; 0]);
    assert_eq!(order_states(&reopened), vec![SwapOrderState::Open; 2]);
    assert!(fixture.reserved(&reopened));
    fixture.finish().await;
}

// The deadline guard reverts a pre-hook after its order's `validTo`. Effects in a block past
// the first order's `validTo` and within the second's can only be the second pre-hook's.
#[tokio::test]
async fn effects_past_one_orders_valid_to_name_the_other_orders_pre_hook() {
    let fixture = Fixture::start().await;
    record_pair(&fixture, [28, 100]);
    let transaction = fund(&fixture, 30);
    let record = fixture.observe(36, 25).await;
    let ran = SwapObservation {
        block: fixture.chain.lock().unwrap().block(30),
        transaction_hash: Some(transaction),
    };
    let swap = record.swap().unwrap();
    let [first, second] = swap.orders() else {
        panic!("the speed-up is the second order");
    };
    assert_eq!(
        (
            first.observations().pre_hook_executed,
            second.observations().pre_hook_executed
        ),
        (None, Some(ran))
    );
    assert_eq!(
        swap.fundings()
            .iter()
            .map(|funding| funding.observation)
            .collect::<Vec<_>>(),
        vec![ran]
    );
    assert_eq!(
        record.swap_hook_winner(U256::ONE),
        Some(second.pre_hook().payload())
    );
    assert_eq!(
        swap_order_state(swap, second),
        SwapOrderState::PreHookOnly { expired: false }
    );
    // The first order had expired when the funding ran, so it ended without sharing it.
    assert_eq!(
        swap_order_state(swap, first),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    fixture.finish().await;
}

// The nonce passes both pre-hooks' nonce with none of the proof's effects in the page, first
// with the proof's notes unspent and then with them reading as spent. Neither shows a funding.
#[tokio::test]
async fn a_consumed_nonce_or_spent_notes_alone_record_no_funding() {
    for spent in [false, true] {
        let fixture = Fixture::start().await;
        record_pair(&fixture, [100, 110]);
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.nonces.push((30, 2));
            chain.nullifiers_spent = spent.then_some(30);
        }
        let record = fixture.observe(36, 25).await;
        let swap = record.swap().unwrap();
        assert!(swap.fundings().is_empty(), "spent: {spent}");
        assert!(
            swap.orders()
                .iter()
                .all(|order| order.observations().pre_hook_executed.is_none()),
            "spent: {spent}"
        );
        assert_eq!(
            (
                record.swap_hook_winner(U256::ONE),
                record.resolved_swap_hook(U256::ONE)
            ),
            (None, None),
            "spent: {spent}"
        );
        // Unspent notes show that no pre-hook ran, and nothing says what took the nonce.
        let expected = if spent {
            SwapOrderState::Open
        } else {
            SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Unknown)
        };
        assert_eq!(order_states(&record), vec![expected; 2], "spent: {spent}");
        fixture.finish().await;
    }
}

// A funded pair whose orders both expire untraded needs recovery, like one order whose
// pre-hook ran. The single order runs beside the pair, so that admission after the recovery is
// the same for both.
#[tokio::test]
async fn a_funded_pair_that_expires_untraded_recovers_like_one_executed_pre_hook() {
    let (profile, k) = (swap_profile(), U256::ONE);
    let mut admissions = Vec::new();
    for overlap in [false, true] {
        let fixture = Fixture::start().await;
        let swap_use = SwapUseId::first(fixture.operation);
        fixture.record_attempt(0, 40);
        if overlap {
            fixture
                .store
                .record_swap_attempt(
                    fixture.operation,
                    speed_up_attempt(&fixture, 1, 44, uid(0, 40)),
                )
                .unwrap();
        }
        let orders = 1 + usize::from(overlap);
        fund(&fixture, 30);
        let record = fixture.observe(36, 25).await;
        assert_eq!(
            order_states(&record),
            vec![SwapOrderState::PreHookOnly { expired: false }; orders],
            "overlap: {overlap}"
        );

        // A finalized block passes every `validTo` without a trade.
        let record = fixture.observe(47, 36).await;
        let stranded = vec![SwapOrderState::PreHookOnly { expired: true }; orders];
        assert_eq!(order_states(&record), stranded, "overlap: {overlap}");
        let fundings = record.swap().unwrap().fundings().to_vec();
        assert_eq!(fundings.len(), 1, "overlap: {overlap}");
        // A reopened store holds the funding and the states.
        let kept = stored(&fixture);
        assert_eq!(kept, record, "overlap: {overlap}");
        assert_eq!(order_states(&kept), stranded, "overlap: {overlap}");

        // No order can fill, so recovery only resets the sell token's approval, and nothing
        // is left to cancel.
        let calls = crate::swap_recovery_calls(
            &kept,
            &profile,
            EXECUTOR,
            &[(SELL, U256::from(SELL_AMOUNT))],
            at_block(47),
        )
        .unwrap();
        let [reset] = calls.as_slice() else {
            panic!("recovery resets only the sell token's approval");
        };
        let approval =
            broadcaster_core::contracts::railgun::approveCall::abi_decode(&reset.data).unwrap();
        assert_eq!(
            (reset.to, approval.spender, approval.amount),
            (SELL, profile.vault_relayer(), U256::ZERO)
        );
        for nonce in [k, U256::from(2)] {
            assert!(
                crate::swap_cancellation_admitted(&kept, nonce, &profile, at_block(47)).is_err(),
                "overlap: {overlap}"
            );
        }

        // The recovery runs at the next nonce and shields the sell token back, and private
        // sync receives its note.
        let request = ShieldRequest {
            preimage: CommitmentPreimage {
                npk: B256::repeat_byte(0x70),
                token: TokenData::erc20(SELL),
                value: U120::from(SELL_AMOUNT),
            },
            ..post_hook_shield(0)
        };
        let shield = Call {
            to: EXECUTOR,
            value: U256::ZERO,
            data: shieldCall {
                _shieldRequests: vec![request.clone()],
            }
            .abi_encode()
            .into(),
        };
        let recovery = B256::repeat_byte(0x3a);
        fixture
            .store
            .record_issued(
                fixture.operation,
                IssuedExecutorPayload::new(
                    U256::from(2),
                    fixture.delegate,
                    recovery,
                    ExecutorPayloadPurpose::Recovery,
                    ExecutorPayloadContext::new(
                        execute(
                            vec![private_transaction(0x31, 0x32)],
                            vec![reset.clone(), shield],
                            2,
                        ),
                        record.nonce_observation().unwrap(),
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
        fixture.chain.lock().unwrap().nonces.push((50, 3));
        let received = synced(vec![railgun_wallet::WalletUtxo::new(Utxo::new(
            request.preimage.note_with_random([0; 16]),
            0,
            50,
            UtxoSource {
                tx_hash: B256::repeat_byte(0x3c),
                block_number: 50,
                block_timestamp: timestamp(50),
            },
            UtxoCommitmentKind::Shield,
        ))]);
        let record = fixture.observe_synced(56, 47, received.clone()).await;
        assert_eq!(
            crate::payload_outcome(
                &record,
                recovery,
                &crate::ExecutorAttributionEvidence {
                    railgun: fixture.railgun,
                    sync: Some(&received),
                    invalidated_orders: None,
                },
            ),
            Some(crate::ExecutorPayloadOutcome::Executed),
            "overlap: {overlap}"
        );
        let swap = record.swap().unwrap();
        assert_eq!(swap.fundings(), fundings, "overlap: {overlap}");
        if overlap {
            // The recovery names neither pre-hook.
            assert!(
                swap.orders()
                    .iter()
                    .all(|order| order.observations().pre_hook_executed.is_none())
            );
            assert_eq!(
                (record.swap_hook_winner(k), record.resolved_swap_hook(k)),
                (None, None)
            );
        }
        let admission = |swap_use| {
            swap_account_refusal(
                &record,
                1,
                SwapAccountRole::Source,
                swap_use,
                SwapAdmissionEvidence::Fresh,
            )
        };
        let mut states = order_states(&record);
        states.dedup();
        admissions.push((
            states,
            swap.admits_attempt(),
            admission(SwapAccountUse::Claimed(swap_use)),
            admission(SwapAccountUse::New),
        ));
        fixture.finish().await;
    }
    // After the recovery the pair reads and is admitted as the single order is.
    assert_eq!(admissions[1], admissions[0]);
}

// A settlement receipt after the shared funding holds one order's trade and its post-hook's
// private credit. The trade names the order that traded and no pre-hook.
#[tokio::test]
async fn a_receipt_trade_after_shared_funding_completes_the_use_and_names_no_pre_hook() {
    for trader in [0_usize, 1] {
        let fixture = Fixture::start().await;
        let valid_to = [100, 110];
        let uids = record_pair(&fixture, valid_to);
        fund(&fixture, 30);
        fixture.observe(36, 25).await;
        let attempt = u8::try_from(trader).unwrap();
        let logs = settlement_logs(&fixture, attempt, valid_to[trader]);
        fixture.chain.lock().unwrap().nonces.push((40, 3));
        let traded = settle_retry(&fixture, uids[trader], 40, logs)
            .await
            .unwrap();

        let record = fixture.record();
        let swap = record.swap().unwrap();
        let (won, other) = (&swap.orders()[trader], &swap.orders()[1 - trader]);
        assert_eq!(
            (won.observations().traded, won.observations().delivered),
            (Some(traded), Some(traded)),
            "trader {trader}"
        );
        assert_eq!(
            won.observations()
                .settlement_credit
                .map(|credit| credit.private_amount),
            Some(U256::from(BUY_AMOUNT - 24)),
            "trader {trader}"
        );
        assert_eq!(
            swap_order_state(swap, won),
            SwapOrderState::Done,
            "trader {trader}"
        );
        assert_eq!(
            (other.observations().traded, other.observations().delivered),
            (None, None),
            "trader {trader}"
        );
        // The trade ran against the funding, whichever pre-hook made it.
        assert_eq!(swap.fundings().len(), 1, "trader {trader}");
        assert!(
            swap.orders()
                .iter()
                .all(|order| order.observations().pre_hook_executed.is_none()),
            "trader {trader}"
        );
        // One full trade exhausts the exact allowance, so the other order can't fill and the
        // use is complete.
        assert_eq!(
            (swap_order_state(swap, other), swap.funding(other)),
            (SwapOrderState::Superseded, None),
            "trader {trader}"
        );
        assert!(
            swap.admits_attempt(),
            "trader {trader}: the completed use admits another attempt"
        );
        fixture.finish().await;
    }
}

// The shared notes stay reserved while either pre-hook can run. At an unused nonce they are
// released once both pre-hooks are dead at finality. At a resolved nonce they wait for private
// sync, whichever of the orders or a cancellation resolved it.
#[tokio::test]
async fn a_pairs_notes_wait_for_both_pre_hooks_to_die_or_for_private_sync() {
    let fixture = Fixture::start().await;
    record_pair(&fixture, [28, 34]);
    let retry = || {
        fixture.store.record_swap_attempt(
            fixture.operation,
            fixture.attempt(2, 100, BUY, SwapDelivery::Reshield, swap_bounds(), None),
        )
    };
    let expired = SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired);
    // The first order's `validTo` passed at finality. The speed-up's pre-hook can still run.
    let record = fixture.observe(31, 21).await;
    assert_eq!(order_states(&record), vec![expired, SwapOrderState::Open]);
    assert!(fixture.reserved(&record));
    assert!(matches!(
        retry(),
        Err(ExecutorStoreError::SwapAttemptOutstanding)
    ));
    // Both passed with the nonce unused: the notes are released, and a retry is admitted by
    // the rule for any retry and reserves them again.
    let record = fixture.observe(37, 31).await;
    assert_eq!(order_states(&record), vec![expired; 2]);
    assert!(!fixture.reserved(&record));
    let retried = retry().unwrap();
    assert!(fixture.reserved(&retried));
    fixture.finish().await;

    for resolver in ["fill", "cancellation"] {
        let fixture = Fixture::start().await;
        let [first, second] = record_pair(&fixture, [100, 110]);
        let (record, expected) = if resolver == "fill" {
            // The speed-up fills in a settlement that also runs the shared proof.
            settle_on_chain(&fixture, 30, second, 1, 9_975);
            (
                fixture.observe(36, 25).await,
                vec![SwapOrderState::Superseded, SwapOrderState::Done],
            )
        } else {
            // One cancellation takes the shared nonce and invalidates both orders. Private
            // sync shows its fee note spent in the transaction the broadcaster reported.
            let fee = sell_note(4);
            let cancellation = record_cancellation(
                &fixture,
                vec![
                    invalidation(fixture.settlement, first),
                    invalidation(fixture.settlement, second),
                ],
                &fee,
            );
            let sent = B256::repeat_byte(0x3b);
            fixture
                .store
                .record_submission(fixture.operation, cancellation, sent)
                .unwrap();
            {
                let mut chain = fixture.chain.lock().unwrap();
                chain.nonces.push((30, 2));
                chain.invalidated.extend([(first, 30), (second, 30)]);
            }
            let mut spent = railgun_wallet::WalletUtxo::new(fee);
            spent.spent = Some(UtxoSource {
                tx_hash: sent,
                block_number: 30,
                block_timestamp: timestamp(30),
            });
            (
                fixture.observe_synced(36, 25, synced(vec![spent])).await,
                vec![SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation); 2],
            )
        };
        assert_eq!(order_states(&record), expected, "{resolver}");
        assert!(
            fixture.reserved(&record),
            "{resolver}: private sync hasn't reached the resolving block"
        );
        // A restart keeps both attempts' notes out of reach while waiting.
        let restarted = restarted_owner(&fixture);
        let kept = stored(&fixture);
        assert!(fixture.reserved(&kept), "{resolver}");
        assert_eq!(
            restarted
                .inputs_for_record(vec![sell_note(3), sell_note(5)], &kept)
                .unwrap()
                .iter()
                .map(|input| input.position)
                .collect::<Vec<_>>(),
            vec![5],
            "{resolver}"
        );
        restarted.shutdown().await;
        // Once private sync passes the read that resolved the nonce, its spent status is all
        // that keeps a note from being selected.
        let resolved_at = record.nonce_watermark().unwrap().block();
        let settled = fixture
            .store
            .record_synced(fixture.operation, None, resolved_at)
            .unwrap();
        assert!(!fixture.reserved(&settled), "{resolver}");
        assert_eq!(
            fixture
                .owner
                .inputs_for_record(vec![sell_note(3)], &settled)
                .unwrap()
                .len(),
            1,
            "{resolver}"
        );
        fixture.finish().await;
    }
}

// One cancellation at the shared nonce invalidates both live orders and pays its fee from
// other notes. It is reported as executed only on evidence of its own: not when either order's
// trade lands first, and not when the nonce resolves with nothing to tell what took it.
#[tokio::test]
async fn one_cancellation_invalidates_both_live_orders_and_is_claimed_only_on_evidence() {
    let (profile, k) = (swap_profile(), U256::ONE);
    // The order whose trade lands before the cancellation. With `None` the nonce passes
    // with no trade, no private effects and neither order invalidated.
    for trader in [Some(0_usize), Some(1), None] {
        let fixture = Fixture::start().await;
        let uids = record_pair(&fixture, [100, 110]);
        let record = fixture.record();
        let calls = crate::swap_recovery_calls(
            &record,
            &profile,
            EXECUTOR,
            &[(SELL, U256::from(SELL_AMOUNT))],
            at_block(20),
        )
        .unwrap();
        let [cancel_first, cancel_second, reset] = calls.as_slice() else {
            panic!("two invalidations and the approval reset");
        };
        let invalidated =
            GPv2Settlement::invalidateOrderCall::abi_decode(&cancel_first.data).unwrap();
        assert_eq!(
            (cancel_first.to, &invalidated.orderUid[..]),
            (fixture.settlement, &uids[0].0[..])
        );
        assert_invalidates_then_resets(
            &[cancel_second.clone(), reset.clone()],
            fixture.settlement,
            uids[1],
        );
        // The fee review's call bound, two per order, covers the batch.
        assert!(calls.len() <= 2 * record.swap().unwrap().orders().len());
        crate::swap_cancellation_admitted(&record, k, &profile, at_block(20)).unwrap();
        // The fee comes from a note neither pre-hook reserves.
        let fee = sell_note(4);
        assert_eq!(
            fixture
                .owner
                .inputs_for_recovery(vec![sell_note(3), fee.clone()], &record)
                .unwrap()
                .iter()
                .map(|input| input.position)
                .collect::<Vec<_>>(),
            vec![4]
        );
        let cancellation = record_cancellation(&fixture, calls.clone(), &fee);

        let expected = if let Some(trader) = trader {
            settle_on_chain(
                &fixture,
                30,
                uids[trader],
                u8::try_from(trader).unwrap(),
                9_975,
            );
            let mut states = vec![SwapOrderState::Superseded; 2];
            states[trader] = SwapOrderState::Done;
            states
        } else {
            fixture.chain.lock().unwrap().nonces.push((30, 2));
            vec![SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Unknown); 2]
        };
        let record = fixture.observe(36, 25).await;
        // The record follows the verified trade, or ends both attempts without a cause.
        assert_eq!(order_states(&record), expected, "trader {trader:?}");
        assert_eq!(
            (record.swap_hook_winner(k), record.resolved_swap_hook(k)),
            (None, None),
            "trader {trader:?}"
        );
        let attribution = fixture.owner.attribution(&record).unwrap();
        assert_eq!(
            crate::payload_outcome(&record, cancellation, &attribution.evidence()),
            Some(crate::ExecutorPayloadOutcome::Resolved),
            "trader {trader:?}: the nonce is resolved and the cancellation is not named"
        );
        fixture.finish().await;
    }
}

// Either attempt's post-hook shields the full bought balance to the same recipient, so the
// speed-up's shield can deliver the first order's trade. The executor's balance, the private
// credit and the traded order's approved minimum decide, as for the order's own shield.
#[tokio::test]
async fn either_attempts_shield_delivers_the_first_orders_trade_above_its_minimum() {
    // The attempt whose post-hook ran, what it credited, and whether that delivers.
    for (shielder, credit, delivers) in [(1_u8, 9_975, true), (0, 9_975, true), (1, 9_974, false)] {
        let case = format!("shielder {shielder}, credit {credit}");
        let fixture = Fixture::start().await;
        let [first, _] = record_pair(&fixture, [100, 110]);
        settle_on_chain(&fixture, 40, first, shielder, credit);
        let record = fixture.observe(46, 35).await;
        let orders = record.swap().unwrap().orders();
        let (traded, speed_up) = (orders[0].observations(), orders[1].observations());
        assert!(
            traded.traded.is_some() && speed_up.traded.is_none(),
            "{case}"
        );
        assert_eq!(
            orders[usize::from(shielder)]
                .observations()
                .shielded
                .map(|shield| shield.private_amount),
            Some(U256::from(credit)),
            "{case}"
        );
        assert_eq!(traded.delivered.is_some(), delivers, "{case}");
        assert_eq!(
            state(&record, 0) == SwapOrderState::Done,
            delivers,
            "{case}"
        );
        assert_eq!(traded.undelivered, None, "{case}");
        fixture.finish().await;
    }
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
    assert_ne!(requests, [] as [Value; 0]);
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
async fn pending_observations_release_activity_and_never_overwrite_concurrent_changes() {
    // Exercise the account read, receipt-only settlement and a settled account's reuse
    // refresh: none may hold activity over RPC, and none may overwrite a changed local record.
    // The account read is a fact about the chain and is applied to the record as changed;
    // the other two decide from their snapshot and are refused.
    for path in ["account", "settlement", "settled reuse"] {
        let fixture = Fixture::start().await;
        fixture.record_attempt(1, 20);
        let logs = settlement_logs(&fixture, 1, 20);
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 16;
            chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
        }
        if path == "settled reuse" {
            // The order is settled history and both of its hooks' nonces are consumed.
            fixture
                .owner
                .observe_swap_settlement(
                    fixture.operation,
                    uid(1, 20),
                    15,
                    crate::OperationHttpClient::direct_for_tests(),
                )
                .await
                .unwrap();
            fixture.chain.lock().unwrap().nonces.push((15, 3));
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
            match path {
                "settlement" => {
                    owner
                        .observe_swap_settlement(
                            operation,
                            uid(1, 20),
                            15,
                            crate::OperationHttpClient::direct_for_tests(),
                        )
                        .await
                }
                "settled reuse" => owner
                    .reuse_swap_account(
                        operation,
                        15,
                        SwapAccountRole::Source,
                        SwapAccountUse::New,
                        crate::OperationHttpClient::direct_for_tests(),
                    )
                    .await
                    .map(|_| ()),
                _ => owner
                    .reconcile_account(operation, crate::OperationHttpClient::direct_for_tests())
                    .await
                    .map(|_| ()),
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
                owner.reconcile_account(
                    ExecutorOperationId::random().unwrap(),
                    crate::OperationHttpClient::direct_for_tests()
                ),
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
        let outcome = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap();
        if path == "account" {
            outcome.unwrap();
            let record = fixture.record();
            assert!(record.is_hidden(), "the read kept the concurrent change");
            assert_eq!(
                record.nonce_observation(),
                Some(ExecutorNonceObservation::new(
                    fixture.chain.lock().unwrap().block(15),
                    U256::ONE
                ))
            );
        } else {
            let error = outcome.unwrap_err();
            assert!(error.is::<crate::ExecutorRecordChanged>(), "{error:#}");
            assert_eq!(
                fixture.record(),
                changed,
                "late observation changed the record"
            );
        }
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
        .observe_swap_settlement(
            fixture.operation,
            uid(1, 20),
            15,
            crate::OperationHttpClient::direct_for_tests(),
        )
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
        .observe_swap_settlement(
            fixture.operation,
            uid(1, 20),
            15,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let record = fixture.record();
    let order = &record.swap().unwrap().orders()[0];
    assert_eq!(
        swap_order_state(record.swap().unwrap(), order),
        SwapOrderState::Done
    );
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
    let restored = reopened
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == fixture.operation)
        .unwrap();
    assert_eq!(state(&restored, 0), SwapOrderState::Done);
    fixture.chain.lock().unwrap().rpc_requests.clear();
    fixture
        .owner
        .observe_swap_settlement(
            fixture.operation,
            uid(1, 20),
            15,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(fixture.chain.lock().unwrap().rpc_requests, [] as [Value; 0]);
    // Reuse after restart keeps completed order evidence local. A fresh current nonce
    // must clear both old hooks, but we need not attribute their historical execution.
    let restarted = restarted_owner(&fixture);
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 17;
        chain.nonces.push((15, 2));
    }
    let reuse = |role| {
        restarted.reuse_swap_account(
            fixture.operation,
            16,
            role,
            SwapAccountUse::New,
            crate::OperationHttpClient::direct_for_tests(),
        )
    };
    // A stale nonce, still at the post-hook's, can't refresh the settled account.
    assert!(reuse(SwapAccountRole::Source).await.is_err());
    assert_eq!(fixture.record().nonce_observation(), nonce_before);
    fixture.chain.lock().unwrap().nonces.push((15, 3));
    fixture.chain.lock().unwrap().rpc_requests.clear();
    let reused = reuse(SwapAccountRole::Source).await.unwrap();
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
    // The settled account's former role doesn't bind it: it is listed and admitted as another
    // swap's destination through the same refresh, with its order history kept.
    assert!(
        restarted
            .swap_destination_candidates(OTHER_BUY)
            .unwrap()
            .iter()
            .any(|candidate| candidate.operation() == fixture.operation)
    );
    let received = reuse(SwapAccountRole::Destination { token: OTHER_BUY })
        .await
        .unwrap();
    assert_eq!(
        received.delegated().unwrap().observed().nonce(),
        U256::from(3)
    );
    assert_eq!(fixture.record().swap(), checked.swap());
    {
        let chain = fixture.chain.lock().unwrap();
        assert_ne!(chain.rpc_requests, [] as [Value; 0]);
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
        .observe_swap_settlement(
            fixture.operation,
            order,
            15,
            crate::OperationHttpClient::direct_for_tests(),
        )
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
            .observe_swap_settlement(
                fixture.operation,
                uid(1, 20),
                15,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        let record = fixture.record();
        assert_eq!(
            state(&record, 0),
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
            .observe_swap_settlement(
                fixture.operation,
                order,
                15,
                crate::OperationHttpClient::direct_for_tests(),
            )
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
                assert_eq!(state(&record, 0), SwapOrderState::Done);
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
            private: None,
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
            recipient: None,
            message_hash: None,
        },
    )
}

/// The handler message a private order's deposit carries.
const PRIVATE_MESSAGE: &[u8] = b"drain to the destination account, then call its shield";

/// `across_order` as a private delivery: the deposit pays `handler` with `PRIVATE_MESSAGE`, and
/// the receiver is the destination stealth account.
fn private_across_order(
    spoke_pool: Address,
    handler: Address,
) -> (BridgeDelivery, AcrossOrderTerms) {
    let (delivery, terms) = across_order(spoke_pool);
    (
        BridgeDelivery {
            private: Some(BridgePrivateDelivery {
                on_shield_failure: BridgeShieldFailure::KeepOnDestination,
            }),
            ..delivery
        },
        AcrossOrderTerms {
            recipient: Some(handler),
            message_hash: Some(keccak256(PRIVATE_MESSAGE)),
            ..terms
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
        recipient: address_to_bytes32(terms.deposit_recipient(delivery)),
        exclusiveRelayer: address_to_bytes32(terms.exclusive_relayer),
        message: if terms.message_hash.is_some() {
            PRIVATE_MESSAGE.into()
        } else {
            Bytes::new()
        },
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
        .observe_swap_settlement(
            fixture.operation,
            uid(1, 20),
            15,
            crate::OperationHttpClient::direct_for_tests(),
        )
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
    // deposit, paid by someone other than the executor. 5: a private order's signed deposit,
    // which pays the handler with the signed message. 6: that deposit with another message.
    for scenario in 0..7 {
        let fixture = Fixture::start().await;
        let spoke_pool = fixture.config.bridge_profile().unwrap().spoke_pool();
        let (delivery, terms) = if scenario >= 5 {
            private_across_order(spoke_pool, Address::repeat_byte(0x7a))
        } else {
            across_order(spoke_pool)
        };
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
            6 => deposit.message = Bytes::from_static(b"another message"),
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
        if !matches!(scenario, 0 | 5) {
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
            ),
            "scenario {scenario}"
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

#[tokio::test]
async fn an_unconfirmed_settlement_block_hints_what_it_shows_and_records_nothing() {
    // 0: an Across order's signed deposit. 1: its trade without a deposit. 2: the signed
    // deposit, on an endpoint without whole-block receipts. 3: a Reshield order's trade and
    // shield.
    for scenario in 0..4 {
        let fixture = Fixture::start().await;
        let (delivery, terms) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
        let logs = if scenario == 3 {
            fixture.record_attempt(1, 20);
            settlement_logs(&fixture, 1, 20)
        } else {
            fixture.record_delivery_attempt(
                1,
                20,
                BUY,
                SwapDelivery::Bridge(delivery),
                Some(BridgeOrderTerms::Across(terms)),
            );
            let deposit = (scenario != 1).then(|| signed_deposit(delivery, &terms));
            across_settlement_logs(&fixture, &terms, deposit)
        };
        // Block 15 is the head, short of the finality depth of 1.
        let (block, settlement) = {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 15;
            chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
            if scenario == 2 {
                chain.receipt_error = Some(-32601);
            }
            chain.rpc_requests.clear();
            (chain.block(15), chain.transactions.last().unwrap().0)
        };
        let before = fixture.record();
        let hint = fixture
            .owner
            .read_swap_settlement_hint(
                fixture.operation,
                uid(1, 20),
                15,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(
            hint,
            (scenario != 2).then_some(crate::SwapSettlementHint {
                block,
                bought: U256::from(BUY_AMOUNT),
                // The Across order's reshielded surplus is not reported as its shield.
                shielded: (scenario == 3).then_some(U256::from(BUY_AMOUNT - 24)),
                shield_transaction: (scenario == 3).then_some(settlement),
                handoff: (scenario == 0).then_some(crate::SwapHandoffHint {
                    block,
                    deposit_id: Some(U256::from(42)),
                    sent: terms.input_amount,
                }),
            }),
            "scenario {scenario}"
        );
        assert_eq!(fixture.record(), before, "scenario {scenario}");
        assert_block_only_requests(&fixture.chain.lock().unwrap().rpc_requests, block);
        fixture.finish().await;
    }
}

/// Record an Across attempt valid to block `valid_to` and expire it: `validTo` passes at the
/// confirmed block while the nonce stays unused.
async fn expired_across_attempt(
    fixture: &Fixture,
    attempt: u8,
    valid_to: u64,
    delivery: BridgeDelivery,
    terms: AcrossOrderTerms,
) {
    fixture.record_delivery_attempt(
        attempt,
        valid_to,
        BUY,
        SwapDelivery::Bridge(delivery),
        Some(BridgeOrderTerms::Across(terms)),
    );
    let record = fixture.observe(valid_to + 3, valid_to + 1).await;
    assert_eq!(
        state(&record, usize::from(attempt) - 1),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
}

// An expired attempt's Across approval stays for a retry at the same nonce until one finalized
// block shows both the unused nonce and a timestamp past the signed quote's age limit, read
// from the pool at that block. Without that evidence the approval stays.
#[tokio::test]
async fn continuing_approvals_read_the_quote_age_from_one_finalized_block() {
    let fixture = Fixture::start().await;
    let spoke_pool = fixture.config.bridge_profile().unwrap().spoke_pool();
    let (delivery, older) = across_order(spoke_pool);
    expired_across_attempt(&fixture, 1, 20, delivery, older).await;
    let swap_use = fixture.record().swap().unwrap().orders()[0]
        .use_id()
        .unwrap();
    // The lower minimum a retry approving 9,900 at `nonce` must disclose.
    let (owner, operation) = (&fixture.owner, fixture.operation);
    let continuing = move |nonce: u64| async move {
        owner
            .swap_continuing_approvals(
                operation,
                swap_use,
                &SwapDelivery::Bridge(delivery),
                Some(U256::from(nonce)),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap()
            .lower_minimum(U256::from(9_900))
    };

    // The pool's limit can't be read.
    assert_eq!(continuing(1).await, Some(older.output_amount));

    // The finalized block is exactly at the limit, which still permits a deposit.
    {
        let mut chain = fixture.chain.lock().unwrap();
        let boundary = timestamp(chain.head - 1) - u64::from(older.quote_timestamp);
        chain.quote_time_buffer = Some(u32::try_from(boundary).unwrap());
        chain.rpc_requests.clear();
    }
    assert_eq!(continuing(1).await, Some(older.output_amount));
    {
        let chain = fixture.chain.lock().unwrap();
        let mut called = Vec::new();
        for request in &chain.rpc_requests {
            match request["method"].as_str().unwrap() {
                "eth_blockNumber" | "eth_getBlockByNumber" | "eth_getCode" => {}
                "eth_call" => called.push(
                    serde_json::from_value::<Address>(request["params"][0]["to"].clone()).unwrap(),
                ),
                method => panic!("the quote-age read sent {method}"),
            }
        }
        called.sort();
        called.dedup();
        let mut expected = vec![EXECUTOR, spoke_pool];
        expected.sort();
        assert_eq!(called, expected);
    }

    // One finalized block later the quote is too old, with the post-hook's nonce still unused.
    fixture.chain.lock().unwrap().head += 1;
    assert_eq!(continuing(1).await, None);

    // A retry at the old post-hook's nonce passes it, which needs no evidence.
    fixture.chain.lock().unwrap().rpc_requests.clear();
    assert_eq!(continuing(2).await, None);
    assert_eq!(fixture.chain.lock().unwrap().rpc_requests, [] as [Value; 0]);
    fixture.finish().await;
}

/// The logs of a settlement that trades the order `retry` and then runs the post-hook of the
/// expired attempt that signed `terms`: its `deposit` and its surplus shield.
fn retry_settlement_logs(
    fixture: &Fixture,
    retry: OrderUid,
    terms: &AcrossOrderTerms,
    deposit: SpokePool::FundsDeposited,
) -> Vec<(Address, LogData)> {
    let mut logs = across_settlement_logs(fixture, terms, Some(deposit));
    for (address, data) in &mut logs {
        if *address == fixture.settlement {
            *data = trade_log(retry);
        }
    }
    logs
}

/// Put `logs` in `block` as one settlement and read it for the order `retry` from its receipts
/// alone. Returns the trade's observation.
async fn settle_retry(
    fixture: &Fixture,
    retry: OrderUid,
    block: u64,
    logs: Vec<(Address, LogData)>,
) -> eyre::Result<SwapObservation> {
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = block + 1;
        chain.add_addressed_transaction(block, fixture.settlement, Bytes::new(), logs);
        chain.rpc_requests.clear();
    }
    fixture
        .owner
        .observe_swap_settlement(
            fixture.operation,
            retry,
            block,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await?;
    let chain = fixture.chain.lock().unwrap();
    let block = chain.block(block);
    assert_block_only_requests(&chain.rpc_requests, block);
    Ok(SwapObservation {
        block,
        transaction_hash: Some(chain.transactions.last().unwrap().0),
    })
}

#[tokio::test]
async fn a_retrys_settlement_hands_off_with_the_expired_attempts_across_deposit() {
    // A retry signs its hooks at the expired attempt's nonces, so the expired attempt's
    // post-hook can still run in the retry's settlement and deposit under its own terms. The
    // trade is the retry's; the hand-off and the surplus belong to the expired attempt.
    // 0: the expired attempt's deposit and surplus shield. 1: both attempts signed the same
    // terms, so the traded order keeps the hand-off. 2: two expired attempts signed the
    // deposit's terms, and the earliest takes the hand-off. 3: one deposit under each
    // attempt's terms.
    for scenario in 0..4 {
        let fixture = Fixture::start().await;
        let spoke_pool = fixture.config.bridge_profile().unwrap().spoke_pool();
        let (delivery, older) = across_order(spoke_pool);
        let newer = if scenario == 1 {
            older
        } else {
            AcrossOrderTerms {
                output_amount: U256::from(9_900),
                quote_timestamp: older.quote_timestamp + 600,
                fill_deadline: older.fill_deadline + 600,
                ..older
            }
        };
        expired_across_attempt(&fixture, 1, 20, delivery, older).await;
        // The retry's attempt number and `validTo` block, and its settlement's block.
        let (attempt, valid_to, block) = if scenario == 2 {
            expired_across_attempt(&fixture, 2, 26, delivery, older).await;
            (3, 60, 40)
        } else {
            (2, 40, 30)
        };
        let (retry_uid, retry_index) = (uid(attempt, valid_to), usize::from(attempt) - 1);
        fixture.record_delivery_attempt(
            attempt,
            valid_to,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(newer)),
        );
        {
            let record = fixture.record();
            let orders = record.swap().unwrap().orders();
            assert_eq!(
                orders[0].pre_hook().nonce(),
                orders[retry_index].pre_hook().nonce(),
                "the expired attempt's post-hook stays executable"
            );
        }

        // The retry's trade, then the expired attempt's deposit and surplus shield.
        let deposit = signed_deposit(delivery, &older);
        let mut logs = retry_settlement_logs(&fixture, retry_uid, &older, deposit);
        if scenario == 3 {
            let executor_payment = Transfer {
                from: EXECUTOR,
                to: older.spoke_pool,
                value: older.input_amount,
            }
            .encode_log_data();
            logs.extend([
                (BUY, executor_payment),
                (
                    newer.spoke_pool,
                    signed_deposit(delivery, &newer).encode_log_data(),
                ),
            ]);
        }
        let observed = settle_retry(&fixture, retry_uid, block, logs).await;
        if scenario == 3 {
            // Conflicting deposits stay unresolved, and the whole read is refused.
            assert!(observed.is_err(), "scenario {scenario}");
            let record = fixture.record();
            assert!(
                record.swap().unwrap().orders().iter().all(|order| {
                    let observed = order.observations();
                    observed.traded.is_none()
                        && observed.bridge_handoff.is_none()
                        && observed.delivered.is_none()
                }),
                "scenario {scenario}"
            );
            fixture.finish().await;
            continue;
        }
        let traded = observed.unwrap();
        let handoff = SwapBridgeHandoff {
            observation: traded,
            deposit_id: Some(U256::from(42)),
        };

        let record = fixture.record();
        let orders = record.swap().unwrap().orders();
        let (expired, retry) = (orders[0].observations(), orders[retry_index].observations());
        assert_eq!(retry.traded, Some(traded), "the trade is the retry's");
        assert!(
            retry.trade_amounts.is_some(),
            "the retry keeps its trade's amounts"
        );
        assert_eq!(
            (expired.traded, expired.trade_amounts),
            (None, None),
            "no trade is invented for the expired attempt"
        );
        match scenario {
            0 => {
                assert_eq!(
                    (
                        retry.bridge_handoff,
                        retry.delivered,
                        retry.settlement_credit
                    ),
                    (None, None, None),
                    "the retry's terms did not make this deposit"
                );
                assert_eq!(
                    (expired.bridge_handoff, expired.delivered),
                    (Some(handoff), Some(traded)),
                    "the hand-off is the expired attempt's, whose payload made the deposit"
                );
                assert_eq!(
                    expired
                        .settlement_credit
                        .map(|credit| credit.private_amount),
                    Some(U256::from(74)),
                    "surplus follows the matched payload's shield request"
                );
                assert_eq!(
                    record
                        .swap_bridges_to_track()
                        .map(SwapOrderRecord::uid)
                        .collect::<Vec<_>>(),
                    vec![uid(1, 20)],
                    "destination tracking follows the actual deposit"
                );
                let swap = record.swap().unwrap();
                assert_eq!(
                    (
                        swap.trade_order(&orders[0]).map(SwapOrderRecord::uid),
                        swap.delivery_order(&orders[retry_index])
                            .map(SwapOrderRecord::uid)
                    ),
                    (Some(uid(2, 40)), Some(uid(1, 20)))
                );
                // Complete source evidence stops rereads of the settlement block.
                fixture.chain.lock().unwrap().rpc_requests.clear();
                fixture
                    .owner
                    .observe_swap_settlement(
                        fixture.operation,
                        uid(2, 40),
                        30,
                        crate::OperationHttpClient::direct_for_tests(),
                    )
                    .await
                    .unwrap();
                assert_eq!(fixture.chain.lock().unwrap().rpc_requests, [] as [Value; 0]);
                assert_eq!(fixture.record(), record);
                // Check status once the nonce passed both hooks. The executor holds the buy
                // amount again, which alone would read as an undelivered trade.
                {
                    let mut chain = fixture.chain.lock().unwrap();
                    chain.nonces.push((30, 3));
                    chain.nullifiers_spent = Some(30);
                    chain.buy_balance.push((30, BUY_AMOUNT));
                    chain.rpc_requests.clear();
                }
                let record = fixture.observe(33, 31).await;
                let orders = record.swap().unwrap().orders();
                let (expired, retry) = (orders[0].observations(), orders[1].observations());
                assert_eq!(
                    (expired.post_hook_deposit, retry.post_hook_deposit),
                    (Some(traded), None),
                    "only the expired attempt signed these terms, so the deposit names its hook"
                );
                assert_eq!(
                    (retry.delivered, retry.undelivered),
                    (None, None),
                    "the use's delivery is the expired attempt's hand-off"
                );
                assert!(!reads_a_balance(
                    &fixture.chain.lock().unwrap().rpc_requests
                ));
            }
            1 => {
                // The shield in the receipt is the expired attempt's request, so the traded
                // order, which owns the terms, takes no credit from it.
                assert_eq!(
                    state(&record, 1),
                    SwapOrderState::Bridging,
                    "scenario {scenario}"
                );
                assert_eq!(
                    (
                        retry.bridge_handoff,
                        retry.delivered,
                        retry.settlement_credit
                    ),
                    (Some(handoff), Some(traded), None),
                    "scenario {scenario}"
                );
                assert_eq!(
                    (
                        expired.bridge_handoff,
                        expired.delivered,
                        expired.settlement_credit
                    ),
                    (None, None, None),
                    "scenario {scenario}"
                );
                // Check status once the nonce passed both hooks. Either attempt's post-hook
                // could have made the deposit, so neither is named and no page is read to
                // find out.
                {
                    let mut chain = fixture.chain.lock().unwrap();
                    chain.nonces.push((30, 3));
                    chain.nullifiers_spent = Some(30);
                }
                let log_queries = || fixture.chain.lock().unwrap().log_queries;
                let before = log_queries();
                let record = fixture.observe(33, 31).await;
                for order in record.swap().unwrap().orders() {
                    let observed = order.observations();
                    assert_eq!(
                        (
                            observed.post_hook_deposit,
                            observed.shielded,
                            observed.settlement_credit
                        ),
                        (None, None, None),
                        "the hook that ran is not named and no surplus is invented"
                    );
                }
                assert_eq!(
                    (state(&record, 0), state(&record, 1)),
                    (
                        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired),
                        SwapOrderState::Bridging
                    )
                );
                assert_eq!(
                    (
                        record.swap_hook_winner(U256::from(2)),
                        record.resolved_swap_hook(U256::from(2))
                    ),
                    (None, None)
                );
                assert_eq!(
                    log_queries(),
                    before,
                    "the source outcome is complete and both hook nonces are consumed"
                );
                let record = fixture.observe(34, 32).await;
                assert_eq!(log_queries(), before, "the search is not repeated");
                let observations = |record: &ExecutorRecord| {
                    record
                        .swap()
                        .unwrap()
                        .orders()
                        .iter()
                        .map(SwapOrderRecord::observations)
                        .collect::<Vec<_>>()
                };
                let restarted = restarted_owner(&fixture);
                let reopened = restarted
                    .observe_swap(
                        fixture.operation,
                        32..34,
                        crate::OperationHttpClient::direct_for_tests(),
                    )
                    .await
                    .unwrap()
                    .record()
                    .clone();
                assert_eq!(log_queries(), before, "nor after a restart");
                assert_eq!(observations(&reopened), observations(&record));
                // The open bridge outcome keeps the account from another swap.
                let reuse = || {
                    restarted.reuse_swap_account(
                        fixture.operation,
                        33,
                        SwapAccountRole::Source,
                        SwapAccountUse::New,
                        crate::OperationHttpClient::direct_for_tests(),
                    )
                };
                let refused = reuse().await.unwrap_err();
                assert_eq!(
                    refused.downcast_ref::<SwapAccountRefusal>(),
                    Some(&SwapAccountRefusal::BridgeUndelivered),
                    "{refused:#}"
                );
                fixture
                    .store
                    .record_swap_bridge_outcome(
                        fixture.operation,
                        uid(2, 40),
                        SwapBridgeOutcome::DeliveredVerified {
                            block: BlockNumHash::new(500, B256::repeat_byte(0x60)),
                            transaction_hash: B256::repeat_byte(0x61),
                            output_amount: older.output_amount,
                            shielded: false,
                        },
                    )
                    .unwrap();
                let reused = reuse().await.unwrap();
                assert_eq!(reused.expected_pre_hook_nonce(), U256::from(3));
                for order in fixture.record().swap().unwrap().orders() {
                    let observed = order.observations();
                    assert_eq!(
                        (observed.post_hook_deposit, observed.shielded),
                        (None, None),
                        "admission names no hook either"
                    );
                }
                restarted.shutdown().await;
            }
            // Scenario 2; the conflicting deposits of scenario 3 returned above.
            _ => {
                assert_eq!(
                    (expired.bridge_handoff, expired.delivered),
                    (Some(handoff), Some(traded)),
                    "scenario {scenario}"
                );
                for later in [orders[1].observations(), retry] {
                    assert_eq!(
                        (later.bridge_handoff, later.delivered),
                        (None, None),
                        "scenario {scenario}"
                    );
                }
            }
        }
        fixture.finish().await;
    }
}

/// Why `owner` refuses the fixture's account, confirmed at block `confirmed`, to a new swap.
async fn reuse_refusal(
    owner: &ExecutorOwner,
    operation: ExecutorOperationId,
    confirmed: u64,
) -> SwapAccountRefusal {
    let refused = owner
        .reuse_swap_account(
            operation,
            confirmed,
            SwapAccountRole::Source,
            SwapAccountUse::New,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap_err();
    *refused
        .downcast_ref::<SwapAccountRefusal>()
        .unwrap_or_else(|| panic!("{refused:#}"))
}

// A retry traded while the expired attempt's Across post-hook made the deposit. The outcome on
// the destination chain and the refund follow that deposit: both are checked against the
// expired attempt's signed terms and recorded on it, and both attempts read as the bridge's
// state. The account takes another swap only after the delivery.
#[tokio::test]
async fn a_split_swaps_bridge_outcome_follows_the_expired_attempts_deposit() {
    // The expired attempt's state as an attempt of its use, then the retry's.
    let states = |record: &ExecutorRecord| {
        let orders = record.swap().unwrap().orders();
        (
            swap_attempt_state(record, &orders[0]),
            swap_attempt_state(record, &orders[1]),
        )
    };
    let outcomes = |record: &ExecutorRecord| {
        let orders = record.swap().unwrap().orders();
        (
            orders[0].observations().bridge_outcome,
            orders[1].observations().bridge_outcome,
        )
    };
    for (surplus, refunds) in [
        (BridgeSurplus::Reshield, false),
        (BridgeSurplus::KeepInAccount, true),
    ] {
        let variant = format!("{surplus:?}, refunds: {refunds}");
        let fixture = Fixture::start().await;
        let (delivery, older) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
        let delivery = BridgeDelivery {
            surplus,
            ..delivery
        };
        let newer = AcrossOrderTerms {
            output_amount: U256::from(9_900),
            quote_timestamp: older.quote_timestamp + 600,
            fill_deadline: older.fill_deadline + 600,
            ..older
        };
        expired_across_attempt(&fixture, 1, 20, delivery, older).await;
        fixture.record_delivery_attempt(
            2,
            40,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(newer)),
        );
        let mut logs = retry_settlement_logs(
            &fixture,
            uid(2, 40),
            &older,
            signed_deposit(delivery, &older),
        );
        let keeps = surplus == BridgeSurplus::KeepInAccount;
        if keeps {
            // The post-hook keeps the surplus rather than shielding it.
            logs.truncate(logs.len() - 3);
        }
        let traded = settle_retry(&fixture, uid(2, 40), 30, logs).await.unwrap();

        let record = fixture.record();
        let orders = record.swap().unwrap().orders();
        let (expired, retry) = (orders[0].observations(), orders[1].observations());
        assert_eq!(
            (expired.bridge_handoff, expired.delivered, retry.traded),
            (
                Some(SwapBridgeHandoff {
                    observation: traded,
                    deposit_id: Some(U256::from(42)),
                }),
                Some(traded),
                Some(traded)
            ),
            "{variant}"
        );
        assert_eq!(
            (
                expired
                    .settlement_credit
                    .map(|credit| credit.private_amount),
                retry.settlement_credit
            ),
            ((!keeps).then_some(U256::from(74)), None),
            "{variant}"
        );
        assert_eq!(
            states(&record),
            (SwapOrderState::Bridging, SwapOrderState::Bridging),
            "{variant}"
        );
        assert_eq!(state(&record, 1), SwapOrderState::Traded, "{variant}");
        let read_before_trade = record.nonce_observation();

        // Check status once the nonce passed both hooks.
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.nonces.push((30, 3));
            chain.nullifiers_spent = Some(30);
        }
        let record = fixture.observe(33, 31).await;
        assert_eq!(
            states(&record),
            (SwapOrderState::Bridging, SwapOrderState::Bridging),
            "{variant}"
        );
        let (destination, config, server) = across_destination(42161).await;
        let reloaded = || {
            ExecutorStore::new(fixture.db.clone(), fixture.view.clone(), 1)
                .unwrap()
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == fixture.operation)
                .unwrap()
        };

        if refunds {
            // Across refunds the expired attempt's deposit to the stealth account.
            let refund = Transfer {
                from: older.spoke_pool,
                to: EXECUTOR,
                value: older.input_amount,
            };
            let refund_tx = {
                let mut chain = fixture.chain.lock().unwrap();
                chain.add_addressed_transaction(
                    34,
                    older.spoke_pool,
                    Bytes::new(),
                    vec![(older.input_token, refund.encode_log_data())],
                );
                chain.head = 36;
                chain.transactions.last().unwrap().0
            };
            let deposit = format!(
                r#"{{"deposit":{{"status":"expired","depositRefundTxHash":"{refund_tx}","outputAmount":"1","recipient":"{}","destinationChainId":"42161"}}}}"#,
                delivery.receiver,
            );
            let (url, _, stub) =
                super::swap_order::spawn_bridge_stub(move |_| deposit.clone()).await;
            let clients = bridge_clients(&fixture, &url);
            assert_eq!(
                observe_bridge(&fixture, &clients, &config).await,
                Some(SwapBridgeOutcome::Refunding),
                "{variant}"
            );
            let record = fixture.record();
            assert_eq!(
                outcomes(&record),
                (Some(SwapBridgeOutcome::Refunding), None),
                "{variant}"
            );
            assert_eq!(
                states(&record),
                (SwapOrderState::Refunding, SwapOrderState::Refunding),
                "{variant}"
            );
            assert_eq!(
                reuse_refusal(&fixture.owner, fixture.operation, 35).await,
                SwapAccountRefusal::BridgeRefunding,
                "{variant}"
            );

            // After a restart, an explicit check verifies the refund against the expired
            // attempt's deposit and records it there.
            let restarted = restarted_owner(&fixture);
            assert_eq!(
                restarted
                    .check_swap_bridge(
                        fixture.operation,
                        uid(1, 20),
                        &clients,
                        Some(&config),
                        crate::OperationHttpClient::direct_for_tests()
                    )
                    .await
                    .unwrap(),
                Some(SwapBridgeOutcome::Refunding),
                "{variant}"
            );
            let verified = SwapObservation {
                block: fixture.chain.lock().unwrap().block(34),
                transaction_hash: Some(refund_tx),
            };
            let record = reloaded();
            let orders = record.swap().unwrap().orders();
            assert_eq!(
                (
                    orders[0].observations().bridge_refund,
                    orders[1].observations().bridge_refund
                ),
                (Some(verified), None),
                "{variant}"
            );
            assert_eq!(
                states(&record),
                (SwapOrderState::Refunding, SwapOrderState::Refunding),
                "{variant}"
            );
            assert_eq!(
                reuse_refusal(&restarted, fixture.operation, 35).await,
                SwapAccountRefusal::BridgeRefunding,
                "{variant}"
            );
            restarted.shutdown().await;
            stub.abort();
        } else {
            fixture.chain.lock().unwrap().head = 34;
            assert_eq!(
                reuse_refusal(&fixture.owner, fixture.operation, 33).await,
                SwapAccountRefusal::BridgeUndelivered,
                "{variant}"
            );
            // A relayer fills deposit 42 under the expired attempt's terms, which sign an
            // output of 9,800. The retry signed 9,900, and no fill pays that.
            let spoke_pool = config.bridge_profile().unwrap().spoke_pool();
            let expected = {
                let mut chain = destination.lock().unwrap();
                chain.add_addressed_transaction(
                    30,
                    spoke_pool,
                    Bytes::new(),
                    vec![(
                        spoke_pool,
                        filled_relay(delivery, &older, 42).encode_log_data(),
                    )],
                );
                chain.head = 31;
                let (transaction_hash, transaction, _) = &chain.transactions[0];
                SwapBridgeOutcome::DeliveredVerified {
                    block: BlockNumHash::new(30, transaction.block_hash.unwrap()),
                    transaction_hash: *transaction_hash,
                    output_amount: U256::from(9_800),
                    shielded: false,
                }
            };
            let (url, lookups, stub) =
                super::swap_order::spawn_bridge_stub(|_| across_report("filled", REPORTED_AMOUNT))
                    .await;
            let clients = bridge_clients(&fixture, &url);
            assert_eq!(
                observe_bridge(&fixture, &clients, &config).await,
                Some(expected.clone()),
                "{variant}"
            );
            assert!(
                lookups
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(path, _)| path == "/api/deposit?originChainId=1&depositId=42")
            );
            let record = fixture.record();
            assert_eq!(
                outcomes(&record),
                (Some(expected.clone()), None),
                "{variant}"
            );
            assert_eq!(
                states(&record),
                (SwapOrderState::Done, SwapOrderState::Done),
                "{variant}"
            );
            // Both attempts are settled history once the account was read past the trade.
            assert!(record.settled_swaps_at(33), "{variant}");
            // The pair settles at the trade's block and no lower. The account read from
            // before the trade stands in here, so the nonce does not set the cutoff.
            let mut early = serde_json::to_value(&record).unwrap();
            early["nonce_observation"] = serde_json::to_value(read_before_trade).unwrap();
            let early: ExecutorRecord = serde_json::from_value(early).unwrap();
            assert!(
                !early.settled_swaps_at(29) && early.settled_swaps_at(30),
                "{variant}"
            );

            // A restart keeps the outcome, and the account takes another swap at the nonce
            // past both hooks.
            let restarted = restarted_owner(&fixture);
            let record = reloaded();
            assert_eq!(outcomes(&record), (Some(expected), None), "{variant}");
            assert_eq!(
                states(&record),
                (SwapOrderState::Done, SwapOrderState::Done),
                "{variant}"
            );
            let reused = restarted
                .reuse_swap_account(
                    fixture.operation,
                    33,
                    SwapAccountRole::Source,
                    SwapAccountUse::New,
                    crate::OperationHttpClient::direct_for_tests(),
                )
                .await
                .unwrap();
            assert_eq!(reused.expected_pre_hook_nonce(), U256::from(3), "{variant}");
            restarted.shutdown().await;
            stub.abort();
        }
        server.abort();
        fixture.finish().await;
    }
}

/// A relayer fills Across deposit 42 under `terms` on Arbitrum in a finalized block, and the
/// hand-off held by `order` is tracked. Returns the tracked outcome and the verified delivery
/// of `terms` it should be.
async fn track_across_fill(
    fixture: &Fixture,
    order: OrderUid,
    delivery: BridgeDelivery,
    terms: &AcrossOrderTerms,
) -> (Option<SwapBridgeOutcome>, SwapBridgeOutcome) {
    let (destination, config, server) = across_destination(42161).await;
    let spoke_pool = config.bridge_profile().unwrap().spoke_pool();
    let expected = {
        let mut chain = destination.lock().unwrap();
        chain.add_addressed_transaction(
            30,
            spoke_pool,
            Bytes::new(),
            vec![(
                spoke_pool,
                filled_relay(delivery, terms, 42).encode_log_data(),
            )],
        );
        chain.head = 31;
        across_verified(&chain, terms, 0)
    };
    let (url, _, stub) =
        super::swap_order::spawn_bridge_stub(|_| across_report("filled", REPORTED_AMOUNT)).await;
    let clients = bridge_clients(fixture, &url);
    let tracked = fixture
        .owner
        .observe_swap_bridge(
            fixture.operation,
            order,
            &clients,
            Some(&config),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    stub.abort();
    server.abort();
    (tracked, expected)
}

// Two live Across attempts sign their post-hooks at one nonce, so either deposit can follow
// either trade in the settlement. The earlier order trades and the receipt holds the deposit
// the speed-up signed: the trade stays on the earlier order, and the hand-off and the outcome
// on the destination chain follow the speed-up's terms, also after a restart.
#[tokio::test]
async fn a_live_orders_trade_hands_off_with_the_speed_ups_across_deposit() {
    let fixture = Fixture::start().await;
    let spoke_pool = fixture.config.bridge_profile().unwrap().spoke_pool();
    // Attempt numbers only label the fixture's orders. The speed-up is attempt 1, whose
    // post-hook's surplus shield `retry_settlement_logs` puts in the receipt.
    let (delivery, replacement) = across_order(spoke_pool);
    let earlier = AcrossOrderTerms {
        output_amount: U256::from(9_900),
        ..replacement
    };
    let (traded_uid, speed_up_uid) = (uid(2, 100), uid(1, 110));
    fixture.record_delivery_attempt(
        2,
        100,
        BUY,
        SwapDelivery::Bridge(delivery),
        Some(BridgeOrderTerms::Across(earlier)),
    );
    record_bridge_speed_up(
        &fixture,
        1,
        110,
        traded_uid,
        BridgeOrderTerms::Across(replacement),
    );
    let logs = retry_settlement_logs(
        &fixture,
        traded_uid,
        &replacement,
        signed_deposit(delivery, &replacement),
    );
    let traded = settle_retry(&fixture, traded_uid, 30, logs).await.unwrap();

    let record = fixture.record();
    let swap = record.swap().unwrap();
    let [first, speed_up] = swap.orders() else {
        panic!("the speed-up is the second order");
    };
    let (trade, handed) = (first.observations(), speed_up.observations());
    assert_eq!(
        (trade.traded, trade.bridge_handoff, trade.delivered),
        (Some(traded), None, None)
    );
    assert!(trade.trade_amounts.is_some() && trade.settlement_credit.is_none());
    assert_eq!(
        (handed.traded, handed.bridge_handoff, handed.delivered),
        (
            None,
            Some(SwapBridgeHandoff {
                observation: traded,
                deposit_id: Some(U256::from(42)),
            }),
            Some(traded)
        )
    );
    assert_eq!(
        handed.settlement_credit.map(|credit| credit.private_amount),
        Some(U256::from(74)),
        "surplus follows the shield request the speed-up signed"
    );
    assert_eq!(
        (
            swap.delivery_order(first).map(SwapOrderRecord::uid),
            swap.trade_order(speed_up).map(SwapOrderRecord::uid)
        ),
        (Some(speed_up_uid), Some(traded_uid))
    );
    assert_eq!(
        record
            .swap_bridges_to_track()
            .map(SwapOrderRecord::uid)
            .collect::<Vec<_>>(),
        vec![speed_up_uid]
    );

    // A relayer fills the deposit under the speed-up's terms.
    let (tracked, expected) =
        track_across_fill(&fixture, speed_up_uid, delivery, &replacement).await;
    assert_eq!(tracked, Some(expected.clone()));
    let record = stored(&fixture);
    let orders = record.swap().unwrap().orders();
    assert_eq!(
        (
            orders[0].observations().bridge_outcome,
            orders[1].observations().bridge_outcome
        ),
        (None, Some(expected))
    );
    for order in orders {
        assert_eq!(swap_attempt_state(&record, order), SwapOrderState::Done);
    }
    fixture.finish().await;
}

// An attempt approved for a destination minimum of 9,000 expired unused, a retry approved for
// 10,000 is open, and a speed-up approved for 9,500 trades. The receipt's deposit is the one
// the first attempt's post-hook signed, so the hand-off and the verified delivery of 9,000 are
// that attempt's. The trade, its amounts and the surplus credit keep their own evidence, and
// nothing records the speed-up's 9,500 as delivered.
#[tokio::test]
async fn a_speed_ups_trade_delivers_under_the_expired_attempts_lower_across_terms() {
    let fixture = Fixture::start().await;
    let (delivery, terms) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
    let signed = |output: u64, later: u32| AcrossOrderTerms {
        output_amount: U256::from(output),
        quote_timestamp: terms.quote_timestamp + later,
        fill_deadline: terms.fill_deadline + later,
        ..terms
    };
    let (original, higher, lower) = (signed(9_000, 0), signed(10_000, 600), signed(9_500, 1_200));
    let (original_uid, retry_uid, speed_up_uid) = (uid(1, 20), uid(2, 60), uid(3, 70));
    expired_across_attempt(&fixture, 1, 20, delivery, original).await;
    fixture.record_delivery_attempt(
        2,
        60,
        BUY,
        SwapDelivery::Bridge(delivery),
        Some(BridgeOrderTerms::Across(higher)),
    );
    record_bridge_speed_up(&fixture, 3, 70, retry_uid, BridgeOrderTerms::Across(lower));
    let logs = retry_settlement_logs(
        &fixture,
        speed_up_uid,
        &original,
        signed_deposit(delivery, &original),
    );
    let traded = settle_retry(&fixture, speed_up_uid, 30, logs)
        .await
        .unwrap();

    let record = fixture.record();
    let swap = record.swap().unwrap();
    let [expired, retry, speed_up] = swap.orders() else {
        panic!("three attempts of one use");
    };
    // The speed-up relaxed the open retry's fill limit, whatever its destination minimum.
    assert!(speed_up.bounds().buy_amount < retry.bounds().buy_amount);
    let (delivering, trading) = (expired.observations(), speed_up.observations());
    assert_eq!(
        (trading.traded, trading.bridge_handoff, trading.delivered),
        (Some(traded), None, None)
    );
    assert_eq!(
        trading
            .trade_amounts
            .map(|amounts| (amounts.sell_amount, amounts.buy_amount)),
        Some((U256::from(SELL_AMOUNT), U256::from(BUY_AMOUNT)))
    );
    assert_eq!(trading.settlement_credit, None);
    assert_eq!(
        (
            delivering.traded,
            delivering.trade_amounts,
            delivering.bridge_handoff,
            delivering.delivered
        ),
        (
            None,
            None,
            Some(SwapBridgeHandoff {
                observation: traded,
                deposit_id: Some(U256::from(42)),
            }),
            Some(traded)
        )
    );
    assert_eq!(
        delivering
            .settlement_credit
            .map(|credit| credit.private_amount),
        Some(U256::from(74))
    );
    assert_eq!(retry.observations(), SwapOrderObservations::default());
    assert_eq!(
        (
            swap.delivery_order(speed_up).map(SwapOrderRecord::uid),
            swap.trade_order(expired).map(SwapOrderRecord::uid)
        ),
        (Some(original_uid), Some(speed_up_uid))
    );

    // A relayer fills the deposit under the first attempt's terms, which sign 9,000.
    let (tracked, expected) = track_across_fill(&fixture, original_uid, delivery, &original).await;
    assert_eq!(tracked, Some(expected));
    let record = stored(&fixture);
    let [expired, retry, speed_up] = record.swap().unwrap().orders() else {
        panic!("three attempts of one use");
    };
    let Some(SwapBridgeOutcome::DeliveredVerified { output_amount, .. }) =
        expired.observations().bridge_outcome
    else {
        panic!("the delivery is verified on the attempt that holds the hand-off");
    };
    assert_eq!(
        (output_amount, Some(output_amount)),
        (original.output_amount, expired.bounds().destination_minimum)
    );
    for later in [retry, speed_up] {
        assert_eq!(later.observations().bridge_outcome, None);
        assert!(Some(output_amount) < later.bounds().destination_minimum);
    }
    for order in [expired, speed_up] {
        assert_eq!(swap_attempt_state(&record, order), SwapOrderState::Done);
    }
    fixture.finish().await;
}

/// Record a NEAR Intents order to BNB Chain whose trade pays `deposit_address`.
fn record_near_order(fixture: &Fixture, deposit_address: Address) {
    record_near_delivery(
        fixture,
        SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::NearIntents,
            destination_chain: 56,
            receiver: Address::repeat_byte(0x77),
            destination_token: Address::ZERO,
            surplus: BridgeSurplus::BridgedByProvider,
            private: None,
        }),
        deposit_address,
    );
}

/// [`record_near_order`] for another `delivery` that 1Click delivers.
fn record_near_delivery(fixture: &Fixture, delivery: SwapDelivery, deposit_address: Address) {
    fixture.record_delivery_attempt(
        1,
        20,
        BUY,
        delivery,
        Some(BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
            deposit_address,
            min_amount_out: U256::from(15),
            amount_out: U256::from(16),
            deadline: "2026-09-30T01:00:00.000Z".into(),
            signed_quote: "{}".into(),
        })),
    );
}

/// Record [`record_near_order`]'s order and confirm its settlement in block 15. Returns the
/// trade's observation.
async fn near_handoff(fixture: &Fixture, deposit_address: Address) -> SwapObservation {
    record_near_order(fixture, deposit_address);
    settle_near_order(fixture, deposit_address).await
}

/// Confirm in block 15 the settlement of the recorded NEAR Intents order that pays
/// `deposit_address`. Returns the trade's observation.
async fn settle_near_order(fixture: &Fixture, deposit_address: Address) -> SwapObservation {
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

/// Chain `chain_id`, an Across order's destination, on its own mock chain at head 30 with
/// finality depth 1.
async fn across_destination(
    chain_id: u64,
) -> (
    Arc<Mutex<MockChain>>,
    crate::settings::EffectiveChainConfig,
    tokio::task::JoinHandle<()>,
) {
    let chain = Arc::new(Mutex::new(MockChain {
        head: 30,
        ..MockChain::new(chain_id, Address::ZERO, Address::ZERO)
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
            .get(chain_id)
            .cloned()
            .unwrap();
    config.enabled = true;
    config.finality_depth = 1;
    config.rpc_route = crate::RpcChainRoute::new(chain_id, vec![endpoint]);
    (chain, config, server)
}

/// Base, which has no pinned `SpokePool`.
const UNPINNED_CHAIN: u64 = 8453;
/// The fill transaction and output amount Across names in [`across_report`].
const REPORTED_FILL: B256 = B256::repeat_byte(0xf1);
const REPORTED_AMOUNT: u64 = 9_801;

/// Record a public Across order to `chain_id` and hand it off in block 15.
async fn across_handoff(fixture: &Fixture, chain_id: u64) -> (BridgeDelivery, AcrossOrderTerms) {
    let (delivery, terms) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
    let delivery = BridgeDelivery {
        destination_chain: chain_id,
        ..delivery
    };
    fixture.record_delivery_attempt(
        1,
        20,
        BUY,
        SwapDelivery::Bridge(delivery),
        Some(BridgeOrderTerms::Across(terms)),
    );
    let logs = across_settlement_logs(fixture, &terms, Some(signed_deposit(delivery, &terms)));
    confirm_settlement(fixture, logs).await;
    (delivery, terms)
}

/// Across's record of the deposit with `status`, naming a fill of `amount` in block 30.
fn across_report(status: &str, amount: u64) -> String {
    format!(
        r#"{{"deposit":{{"status":"{status}","fillBlockNumber":30,"fillTx":"{REPORTED_FILL}","outputAmount":"{amount}","recipient":"{}","destinationChainId":"42161"}}}}"#,
        Address::repeat_byte(0x78),
    )
}

/// The delivery [`across_report`] reports without evidence on the destination chain.
fn across_reported() -> SwapBridgeOutcome {
    SwapBridgeOutcome::DeliveredReported {
        amount_out: Some(U256::from(REPORTED_AMOUNT)),
        transaction_hash: Some(REPORTED_FILL),
        destination_transaction: None,
    }
}

/// The verified delivery of `terms` by the destination chain's transaction `index`.
fn across_verified(chain: &MockChain, terms: &AcrossOrderTerms, index: usize) -> SwapBridgeOutcome {
    let (transaction_hash, transaction, _) = &chain.transactions[index];
    SwapBridgeOutcome::DeliveredVerified {
        block: BlockNumHash::new(30, transaction.block_hash.unwrap()),
        transaction_hash: *transaction_hash,
        output_amount: terms.output_amount,
        shielded: false,
    }
}

/// The order's recorded bridge outcome.
fn bridge_outcome(record: &ExecutorRecord) -> Option<SwapBridgeOutcome> {
    record.swap().unwrap().orders()[0]
        .observations()
        .bridge_outcome
}

/// A relayer's fill of Across deposit `deposit_id` from chain 1 with the order's terms.
fn filled_relay(
    delivery: BridgeDelivery,
    terms: &AcrossOrderTerms,
    deposit_id: u64,
) -> SpokePool::FilledRelay {
    let recipient = address_to_bytes32(terms.deposit_recipient(delivery));
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
        messageHash: terms.deposit_message_hash(),
        relayExecutionInfo: V3RelayExecutionEventInfo {
            updatedRecipient: recipient,
            updatedMessageHash: terms.deposit_message_hash(),
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
        .observe_swap_bridge(
            fixture.operation,
            uid(1, 20),
            clients,
            Some(destination),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn across_delivery_is_verified_from_finalized_destination_receipts() {
    // 0: the deposit's fill. 1: a fill of another deposit. 2: the fill's block is reorged
    // during the read. 3: Across reports the deposit expired. 4: a slow fill, which pays more
    // than the signed output. 5: a fill that executes less than the signed output.
    // A private order, whose record without a message is scenario 0: 6: the fill, the
    // handler's transfer to the destination stealth account and its shield. 7: the fill and
    // that transfer, without the shield. 8: a fill executed with another message. 9: Across
    // reports the deposit expired. 10: the fill, which doesn't reach the destination stealth
    // account.
    for scenario in 0..11 {
        let fixture = Fixture::start().await;
        let (destination, config, server) = across_destination(42161).await;
        let profile = config.bridge_profile().unwrap();
        let (spoke_pool, handler) = (profile.spoke_pool(), profile.multicall_handler());
        let railgun = config.require_railgun().unwrap().deployment.contract;
        let origin_pool = fixture.config.bridge_profile().unwrap().spoke_pool();
        let private = scenario >= 6;
        let (delivery, terms) = if private {
            private_across_order(origin_pool, handler)
        } else {
            across_order(origin_pool)
        };
        fixture.record_delivery_attempt(
            1,
            20,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(terms)),
        );
        let logs = across_settlement_logs(&fixture, &terms, Some(signed_deposit(delivery, &terms)));
        confirm_settlement(&fixture, logs).await;
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
        if scenario == 8 {
            fill.relayExecutionInfo.updatedMessageHash = B256::ZERO;
        }
        // The handler passes its whole balance on, which can exceed the fill.
        let received = executed + U256::from(7);
        let transfer = |from: Address, to: Address, value: U256| {
            (
                terms.output_token,
                Transfer { from, to, value }.encode_log_data(),
            )
        };
        let mut logs = vec![(spoke_pool, fill.encode_log_data())];
        if private && scenario != 10 {
            logs.push(transfer(handler, delivery.receiver, received));
            if scenario != 7 {
                // The shield moves the net amount to Railgun and the fee to its treasury.
                logs.extend([
                    transfer(delivery.receiver, railgun, received - U256::from(25)),
                    transfer(
                        delivery.receiver,
                        Address::repeat_byte(0xfe),
                        U256::from(25),
                    ),
                ]);
            }
        }
        {
            let mut chain = destination.lock().unwrap();
            chain.add_addressed_transaction(30, spoke_pool, Bytes::new(), logs);
            chain.reorg_on_receipts = scenario == 2;
        }
        // Across's fill transaction, amount and recipient only locate the block.
        let expired = matches!(scenario, 3 | 9);
        let status = if expired { "expired" } else { "filled" };
        let deposit = format!(
            r#"{{"deposit":{{"status":"{status}","fillBlockNumber":30,"fillTx":"{}","outputAmount":"1","recipient":"{}","destinationChainId":"42161"}}}}"#,
            B256::repeat_byte(0xf1),
            Address::repeat_byte(0x78),
        );
        let (url, lookups, stub) =
            super::swap_order::spawn_bridge_stub(move |_| deposit.clone()).await;
        let clients = bridge_clients(&fixture, &url);
        let first = observe_bridge(&fixture, &clients, &config).await;
        let outcome = if expired {
            // The destination chain isn't read.
            assert_eq!(first, Some(SwapBridgeOutcome::Refunding));
            assert_eq!(destination.lock().unwrap().rpc_requests, [] as [Value; 0]);
            first
        } else {
            // The fill's block isn't final at head 30.
            assert_eq!(first, None, "scenario {scenario}");
            destination.lock().unwrap().head = 31;
            let outcome = observe_bridge(&fixture, &clients, &config).await;
            let chain = destination.lock().unwrap();
            let (transaction_hash, transaction, _) = &chain.transactions[0];
            let block = BlockNumHash::new(30, transaction.block_hash.unwrap());
            let transaction_hash = *transaction_hash;
            // The executed amount is delivered. A private delivery records the amount the
            // destination stealth account received, shielded or held.
            let expected = match scenario {
                0 | 4 => Some(SwapBridgeOutcome::DeliveredVerified {
                    block,
                    transaction_hash,
                    output_amount: executed,
                    shielded: false,
                }),
                6 => Some(SwapBridgeOutcome::DeliveredVerified {
                    block,
                    transaction_hash,
                    output_amount: received,
                    shielded: true,
                }),
                7 => Some(SwapBridgeOutcome::HeldOnDestination {
                    block,
                    transaction_hash,
                    amount: received,
                }),
                // The final block holds no fill of the deposit, so a public delivery is
                // Across's word, with the amount and transaction it names.
                1 | 5 => Some(SwapBridgeOutcome::DeliveredReported {
                    amount_out: Some(U256::ONE),
                    transaction_hash: Some(B256::repeat_byte(0xf1)),
                    destination_transaction: None,
                }),
                // A private delivery is never reported.
                _ => None,
            };
            assert_eq!(outcome, expected, "scenario {scenario}");
            assert_block_only_requests(&chain.rpc_requests, block);
            outcome
        };
        if scenario == 10 {
            // Nor is it reported when no endpoint serves whole-block receipts.
            destination.lock().unwrap().receipt_error = Some(-32601);
            assert!(
                fixture
                    .owner
                    .observe_swap_bridge(
                        fixture.operation,
                        uid(1, 20),
                        &clients,
                        Some(&config),
                        crate::OperationHttpClient::direct_for_tests()
                    )
                    .await
                    .is_err()
            );
        }
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
                SwapOrderState::Done,
                SwapOrderState::Bridging,
                SwapOrderState::Refunding,
                SwapOrderState::Done,
                SwapOrderState::Done,
                SwapOrderState::Done,
                SwapOrderState::HeldOnDestination,
                SwapOrderState::Bridging,
                SwapOrderState::Refunding,
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
    let (destination, config, server) = across_destination(42161).await;
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
        .check_swap_bridge(
            fixture.operation,
            uid(1, 20),
            &clients,
            Some(&config),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let expected = {
        let chain = destination.lock().unwrap();
        let (transaction_hash, transaction, _) = &chain.transactions[0];
        SwapBridgeOutcome::DeliveredVerified {
            block: BlockNumHash::new(30, transaction.block_hash.unwrap()),
            transaction_hash: *transaction_hash,
            output_amount: terms.output_amount,
            shielded: false,
        }
    };
    assert_eq!(outcome, Some(expected.clone()));
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
                .check_swap_bridge(
                    fixture.operation,
                    uid(1, 20),
                    &clients,
                    Some(&arbitrum),
                    crate::OperationHttpClient::direct_for_tests()
                )
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
                        shielded: false,
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

/// Anyone can emit a matching fill event, so one counts only from the destination chain's pinned
/// `SpokePool`, or beside the output token's transfer of the signed amount to the receiver in
/// the same receipt, which then counts once for the amount transferred. A final block without
/// such a fill leaves the delivery as Across's report.
#[tokio::test]
async fn across_fills_need_the_pinned_pool_or_the_token_transfer_and_are_otherwise_reported() {
    enum Expected {
        /// By the destination chain's transaction at this index, for this much above the signed
        /// amount.
        Verified(usize, u64),
        Reported,
        Ambiguous,
    }
    /// What a forged event claims to have executed above the signed amount.
    const FORGED: u64 = 1_000_000;
    let (_, terms) = across_order(Address::ZERO);
    let receiver = Address::repeat_byte(0x77);
    let (emitter, relayer) = (Address::repeat_byte(0x5e), Address::repeat_byte(0x55));
    let other_emitter = Address::repeat_byte(0x5f);
    let pinned =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(42161)
            .unwrap()
            .bridge_destination()
            .unwrap()
            .spoke_pool()
            .unwrap();
    let paid = |token: Address, value: U256| {
        (
            token,
            Transfer {
                from: relayer,
                to: receiver,
                value,
            }
            .encode_log_data(),
        )
    };
    let payment = paid(terms.output_token, terms.output_amount);
    // Each receipt of the fill block as its fill events, by emitter and the amount executed
    // above the signed one, and the transfers after them.
    for (scenario, (chain_id, receipts, expected)) in [
        // A chain without a pinned pool, and one whose pinned pool no longer emits the fill.
        (
            UNPINNED_CHAIN,
            vec![(vec![(emitter, 0)], vec![payment.clone()])],
            Expected::Verified(0, 0),
        ),
        (
            42161,
            vec![(vec![(emitter, 0)], vec![payment.clone()])],
            Expected::Verified(0, 0),
        ),
        // No transfer, as for a native delivery: the fill event alone proves nothing.
        (
            UNPINNED_CHAIN,
            vec![(vec![(emitter, 0)], vec![])],
            Expected::Reported,
        ),
        // Less than the signed amount, and the signed amount of another token.
        (
            UNPINNED_CHAIN,
            vec![(
                vec![(emitter, 0)],
                vec![paid(terms.output_token, terms.output_amount - U256::ONE)],
            )],
            Expected::Reported,
        ),
        (
            UNPINNED_CHAIN,
            vec![(
                vec![(emitter, 0)],
                vec![paid(OTHER_BUY, terms.output_amount)],
            )],
            Expected::Reported,
        ),
        // The signed amount of the output token, paid to someone other than the receiver.
        (
            UNPINNED_CHAIN,
            vec![(
                vec![(emitter, 0)],
                vec![(
                    terms.output_token,
                    Transfer {
                        from: relayer,
                        to: Address::repeat_byte(0x78),
                        value: terms.output_amount,
                    }
                    .encode_log_data(),
                )],
            )],
            Expected::Reported,
        ),
        // Two receipts with evidence.
        (
            UNPINNED_CHAIN,
            vec![
                (vec![(emitter, 0)], vec![payment.clone()]),
                (vec![(other_emitter, 0)], vec![payment.clone()]),
            ],
            Expected::Ambiguous,
        ),
        // A forged event without evidence doesn't make the real fill ambiguous.
        (
            42161,
            vec![(vec![(emitter, 0)], vec![]), (vec![(pinned, 0)], vec![])],
            Expected::Verified(1, 0),
        ),
        // Nor does one in the real fill's receipt: its events are one candidate, for the
        // smallest qualifying transfer and not for an amount an event claims.
        (
            UNPINNED_CHAIN,
            vec![(
                vec![(emitter, 0), (other_emitter, FORGED)],
                vec![
                    paid(terms.output_token, terms.output_amount + U256::from(50)),
                    paid(terms.output_token, terms.output_amount + U256::from(3)),
                ],
            )],
            Expected::Verified(0, 3),
        ),
        // The pinned pool's event gives the amount, whatever is logged before it.
        (
            42161,
            vec![(vec![(emitter, FORGED), (pinned, 5)], vec![payment.clone()])],
            Expected::Verified(0, 5),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::start().await;
        let (delivery, terms) = across_handoff(&fixture, chain_id).await;
        let (destination, config, server) = across_destination(chain_id).await;
        {
            let mut chain = destination.lock().unwrap();
            for (events, transfers) in receipts {
                let to = events[0].0;
                let mut logs = events
                    .into_iter()
                    .map(|(emitter, above)| {
                        let mut fill = filled_relay(delivery, &terms, 42);
                        fill.relayExecutionInfo.updatedOutputAmount += U256::from(above);
                        (emitter, fill.encode_log_data())
                    })
                    .collect::<Vec<_>>();
                logs.extend(transfers);
                chain.add_addressed_transaction(30, to, Bytes::new(), logs);
            }
            chain.head = 31;
        }
        let (url, _, stub) =
            super::swap_order::spawn_bridge_stub(|_| across_report("filled", REPORTED_AMOUNT))
                .await;
        let clients = bridge_clients(&fixture, &url);
        let result = fixture
            .owner
            .observe_swap_bridge(
                fixture.operation,
                uid(1, 20),
                &clients,
                Some(&config),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await;
        let (outcome, expected_state) = {
            let chain = destination.lock().unwrap();
            assert_block_only_requests(&chain.rpc_requests, chain.block(30));
            match expected {
                Expected::Verified(index, above) => {
                    let mut verified = across_verified(&chain, &terms, index);
                    if let SwapBridgeOutcome::DeliveredVerified { output_amount, .. } =
                        &mut verified
                    {
                        *output_amount += U256::from(above);
                    }
                    (Some(verified), SwapOrderState::Done)
                }
                Expected::Reported => (Some(across_reported()), SwapOrderState::Done),
                Expected::Ambiguous => (None, SwapOrderState::Bridging),
            }
        };
        match expected {
            Expected::Ambiguous => assert!(result.is_err(), "scenario {scenario}"),
            _ => assert_eq!(result.unwrap(), outcome, "scenario {scenario}"),
        }
        let record = fixture.record();
        assert_eq!(bridge_outcome(&record), outcome, "scenario {scenario}");
        assert_eq!(state(&record, 0), expected_state, "scenario {scenario}");
        // A report is final for routine polling, like a verified delivery.
        assert_eq!(
            record.swap_bridges_to_track().count(),
            usize::from(outcome.is_none()),
            "scenario {scenario}"
        );
        stub.abort();
        server.abort();
        fixture.finish().await;
    }
}

/// A report is final, so only a read that establishes the fill block holds no fill, or that
/// every Sensitive endpoint lacks whole-block receipts, yields one. A read that could still turn
/// out otherwise leaves the swap to be checked again. An endpoint whose identity check passes
/// after the first ones were tried is still asked. A General-only endpoint is never asked.
#[tokio::test]
async fn across_reports_a_delivery_only_when_no_later_read_could_verify_it() {
    // The final fill block holds a fill event without evidence, as for a native delivery. 0: it
    // leaves the chain during the read. 1: the chain's only endpoint has no whole-block
    // receipts. 2: one endpoint has none, and another fails to serve them.
    // The identity check of a second endpoint is held until the first, which has no
    // whole-block receipts, was asked for them. 3: the second has none either. 4: the second
    // serves them, and the fill has its token transfer. 5: the check stays held. 6: like 1,
    // with a General-only endpoint that serves them. 7: like 5, with that endpoint too.
    for scenario in 0..8 {
        let fixture = Fixture::start().await;
        let (delivery, terms) = across_handoff(&fixture, UNPINNED_CHAIN).await;
        let (destination, mut config, server) = across_destination(UNPINNED_CHAIN).await;
        {
            let emitter = Address::repeat_byte(0x5e);
            let mut logs = vec![(
                emitter,
                filled_relay(delivery, &terms, 42).encode_log_data(),
            )];
            if scenario == 4 {
                let payment = Transfer {
                    from: Address::repeat_byte(0x55),
                    to: delivery.receiver,
                    value: terms.output_amount,
                };
                logs.push((terms.output_token, payment.encode_log_data()));
            }
            let mut chain = destination.lock().unwrap();
            chain.add_addressed_transaction(30, emitter, Bytes::new(), logs);
            chain.head = 31;
            chain.reorg_on_receipts = scenario == 0;
            if scenario == 1 {
                chain.receipt_error = Some(-32601);
            }
        }
        let mut servers = vec![server];
        // Per endpoint, the error it answers whole-block receipts with, whether its identity
        // check is held, and whether it is Sensitive.
        let endpoints: &[(Option<i64>, bool, bool)] = match scenario {
            2 => &[(Some(-32601), false, true), (Some(-32000), false, true)],
            3 | 5 => &[(Some(-32601), false, true), (Some(-32601), true, true)],
            4 => &[(Some(-32601), false, true), (None, true, true)],
            6 => &[(Some(-32601), false, true), (None, false, false)],
            7 => &[
                (Some(-32601), false, true),
                (Some(-32601), true, true),
                (None, false, false),
            ],
            _ => &[],
        };
        let general_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        if !endpoints.is_empty() {
            let gate = crate::rpc_broker::tests::RpcMockGate {
                request_started: Arc::default(),
                release_response: Arc::default(),
            };
            let asked = Arc::new(tokio::sync::Notify::new());
            let mut urls = Vec::new();
            for &(code, held, sensitive) in endpoints {
                let served = destination.clone();
                let asked = asked.clone();
                let general_requests = general_requests.clone();
                let responder: crate::rpc_broker::tests::RpcResponder = Arc::new(
                    move |request: Value| {
                        if !sensitive {
                            general_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        let result = served.lock().unwrap().respond(&request);
                        if request["method"] != "eth_getBlockReceipts" {
                            return result;
                        }
                        asked.notify_one();
                        code.map_or(result, |code| {
                            json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":code, "message":"unavailable"}})
                        })
                    },
                );
                let (endpoint, server) = if held {
                    crate::rpc_broker::tests::spawn_gated_rpc_mock(
                        responder,
                        Arc::default(),
                        Arc::default(),
                        gate.clone(),
                    )
                    .await
                } else {
                    crate::rpc_broker::tests::spawn_rpc_mock(
                        responder,
                        Arc::default(),
                        Arc::default(),
                    )
                    .await
                };
                let roles = crate::RpcEndpointRoles {
                    general: !sensitive,
                    sensitive,
                    submit: true,
                };
                urls.push((endpoint.into(), crate::RpcTransport::Wallet, roles));
                servers.push(server);
            }
            config.rpc_route = crate::RpcChainRoute::new_with_transports(UNPINNED_CHAIN, urls);
            if scenario >= 3 {
                config.rpc_route = config.rpc_route.with_identity_verification();
            }
            if matches!(scenario, 3 | 4) {
                // Release the held endpoint's requests once the first was asked for receipts.
                servers.push(tokio::spawn(async move {
                    gate.request_started.notified().await;
                    asked.notified().await;
                    loop {
                        gate.release_response.notify_one();
                        gate.request_started.notified().await;
                    }
                }));
            }
        }
        let (url, _, stub) =
            super::swap_order::spawn_bridge_stub(|_| across_report("filled", REPORTED_AMOUNT))
                .await;
        let clients = bridge_clients(&fixture, &url);
        let result = fixture
            .owner
            .observe_swap_bridge(
                fixture.operation,
                uid(1, 20),
                &clients,
                Some(&config),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await;
        let expected = match scenario {
            1 | 3 | 6 => Some(across_reported()),
            4 => Some(across_verified(&destination.lock().unwrap(), &terms, 0)),
            _ => None,
        };
        // A failed read is an error, and so is an endpoint that didn't answer. The swap is
        // polled again.
        assert_eq!(
            result.is_err(),
            matches!(scenario, 2 | 5 | 7),
            "scenario {scenario}"
        );
        assert_eq!(
            general_requests.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "scenario {scenario}"
        );
        assert_eq!(result.ok().flatten(), expected, "scenario {scenario}");
        let record = fixture.record();
        assert_eq!(bridge_outcome(&record), expected, "scenario {scenario}");
        assert_eq!(
            record.swap_bridges_to_track().count(),
            usize::from(expected.is_none()),
            "scenario {scenario}"
        );
        stub.abort();
        for server in servers {
            server.abort();
        }
        fixture.finish().await;
    }
}

/// Nothing on the destination chain backs a delivery Across only reported, and its refund would
/// land on this chain. Routine polling stops at the report, and an explicit status check asks
/// Across again. A report never replaces an outcome already recorded.
#[tokio::test]
async fn an_explicit_check_of_an_across_report_verifies_it_or_finds_its_refund() {
    // The fill block is final and holds a fill event without evidence. 0: the check then finds
    // a fill with the token transfer. 1: Across now reports the deposit expired. 2: Across
    // reports another amount, and nothing else changed. 3: Across reported the deposit expired
    // first, and filled at the check.
    for scenario in 0..4 {
        let fixture = Fixture::start().await;
        let (delivery, terms) = across_handoff(&fixture, UNPINNED_CHAIN).await;
        let (destination, config, server) = across_destination(UNPINNED_CHAIN).await;
        let emitter = Address::repeat_byte(0x5e);
        let fill = (
            emitter,
            filled_relay(delivery, &terms, 42).encode_log_data(),
        );
        {
            let mut chain = destination.lock().unwrap();
            chain.add_addressed_transaction(30, emitter, Bytes::new(), vec![fill.clone()]);
            chain.head = 31;
        }
        let first = if scenario == 3 { "expired" } else { "filled" };
        let reply = Arc::new(Mutex::new(across_report(first, REPORTED_AMOUNT)));
        let served = reply.clone();
        let (url, lookups, stub) =
            super::swap_order::spawn_bridge_stub(move |_| served.lock().unwrap().clone()).await;
        let clients = bridge_clients(&fixture, &url);
        let known = if scenario == 3 {
            SwapBridgeOutcome::Refunding
        } else {
            across_reported()
        };
        assert_eq!(
            observe_bridge(&fixture, &clients, &config).await,
            Some(known.clone()),
            "scenario {scenario}"
        );
        // Routine polling asks neither Across nor the destination chain again.
        destination.lock().unwrap().rpc_requests.clear();
        assert_eq!(
            observe_bridge(&fixture, &clients, &config).await,
            Some(known.clone())
        );
        assert_eq!(lookups.lock().unwrap().len(), 1);
        assert_eq!(destination.lock().unwrap().rpc_requests, [] as [Value; 0]);
        match scenario {
            0 => {
                let payment = Transfer {
                    from: Address::repeat_byte(0x55),
                    to: delivery.receiver,
                    value: terms.output_amount,
                };
                destination.lock().unwrap().add_addressed_transaction(
                    30,
                    emitter,
                    Bytes::new(),
                    vec![fill, (terms.output_token, payment.encode_log_data())],
                );
            }
            1 => *reply.lock().unwrap() = across_report("expired", REPORTED_AMOUNT),
            _ => *reply.lock().unwrap() = across_report("filled", REPORTED_AMOUNT + 1),
        }
        let checked = fixture
            .owner
            .check_swap_bridge(
                fixture.operation,
                uid(1, 20),
                &clients,
                Some(&config),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(lookups.lock().unwrap().len(), 2);
        let (expected, expected_state) = match scenario {
            0 => (
                across_verified(&destination.lock().unwrap(), &terms, 1),
                SwapOrderState::Done,
            ),
            1 | 3 => (SwapBridgeOutcome::Refunding, SwapOrderState::Refunding),
            _ => (known, SwapOrderState::Done),
        };
        assert_eq!(checked, Some(expected.clone()), "scenario {scenario}");
        let record = fixture.record();
        assert_eq!(
            bridge_outcome(&record),
            Some(expected),
            "scenario {scenario}"
        );
        assert_eq!(state(&record, 0), expected_state, "scenario {scenario}");
        stub.abort();
        server.abort();
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
                destination_transaction: None,
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
        if status == "SUCCESS" {
            // 1Click's report is final. Only a delivery Across reported is open to a later
            // check.
            for replacement in [
                SwapBridgeOutcome::Refunding,
                SwapBridgeOutcome::DeliveredVerified {
                    block: BlockNumHash::new(30, B256::repeat_byte(30)),
                    transaction_hash: destination_tx,
                    output_amount: U256::from(16),
                    shielded: false,
                },
            ] {
                assert!(matches!(
                    fixture.store.record_swap_bridge_outcome(
                        fixture.operation,
                        uid(1, 20),
                        replacement
                    ),
                    Err(ExecutorStoreError::InvalidRecord)
                ));
            }
        }
        if expected == Some(SwapBridgeOutcome::NeedsAttention) {
            // Routine polling leaves it alone; an explicit status check asks again.
            assert_eq!(observe_bridge(&fixture, &clients, &bnb).await, expected);
            assert_eq!(requests.lock().unwrap().len(), 1);
            assert_eq!(
                fixture
                    .owner
                    .check_swap_bridge(
                        fixture.operation,
                        uid(1, 20),
                        &clients,
                        Some(&bnb),
                        crate::OperationHttpClient::direct_for_tests()
                    )
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

// A non-EVM delivery is tracked as a NEAR Intents one, by its deposit address alone, and
// without a destination chain: 1Click's success keeps the destination transaction as 1Click
// reports it, a refund returns the deposit to the stealth account, and a failure needs
// attention. No RPC request is made for any of them.
#[tokio::test]
async fn non_evm_reports_set_the_outcome_without_reading_a_chain() {
    let signature =
        "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW";
    for (status, expected, expected_state) in [
        (
            "SUCCESS",
            Some(SwapBridgeOutcome::DeliveredReported {
                amount_out: Some(U256::from(16)),
                transaction_hash: None,
                destination_transaction: Some(signature.to_owned()),
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
    ] {
        let fixture = Fixture::start().await;
        let deposit_address = Address::repeat_byte(0x79);
        record_near_delivery(
            &fixture,
            SwapDelivery::NonEvmBridge(crate::vault::NonEvmDelivery {
                network: crate::bridge::NonEvmNetwork::Solana,
                receiver: "So11111111111111111111111111111111111111112".to_owned(),
                asset_id: "nep141:sol.omft.near".to_owned(),
                contract: None,
                symbol: "SOL".to_owned(),
                decimals: 9,
            }),
            deposit_address,
        );
        settle_near_order(&fixture, deposit_address).await;
        let report = format!(
            r#"{{"status":"{status}","swapDetails":{{"amountOut":"16","destinationChainTxHashes":[{{"hash":"{signature}","explorerUrl":"https://solscan.io/tx/{signature}"}}]}}}}"#
        );
        let (url, requests, stub) =
            super::swap_order::spawn_bridge_stub(move |_| report.clone()).await;
        let clients = bridge_clients(&fixture, &url);
        fixture.chain.lock().unwrap().rpc_requests.clear();
        let outcome = fixture
            .owner
            .observe_swap_bridge(
                fixture.operation,
                uid(1, 20),
                &clients,
                None,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(outcome, expected, "{status}");
        let record = fixture.record();
        assert_eq!(
            record.swap().unwrap().orders()[0]
                .observations()
                .bridge_outcome,
            expected,
            "{status}"
        );
        assert_eq!(state(&record, 0), expected_state, "{status}");
        assert_eq!(requests.lock().unwrap().len(), 1, "{status}");
        assert_eq!(
            fixture.chain.lock().unwrap().rpc_requests,
            [] as [Value; 0],
            "{status}"
        );
        stub.abort();
        fixture.finish().await;
    }
}

#[tokio::test]
async fn a_providers_report_before_the_recorded_handoff_is_returned_and_records_nothing() {
    use crate::SwapBridgeReport::{Delivered, Failed, Pending, Refunded};
    let delivered = |amount: u64, transaction: Option<B256>| Delivered {
        amount_out: Some(U256::from(amount)),
        seen: None,
        transaction,
    };
    // Across is asked about the hinted deposit, and not at all without one. NEAR Intents is
    // asked by the order's deposit address.
    for (across, status, hinted, expected) in [
        (
            true,
            "filled",
            true,
            delivered(REPORTED_AMOUNT, Some(REPORTED_FILL)),
        ),
        (true, "pending", true, Pending),
        (true, "expired", true, Refunded),
        (true, "filled", false, Pending),
        (false, "SUCCESS", false, delivered(16, None)),
        (false, "PROCESSING", false, Pending),
        (false, "FAILED", false, Failed),
    ] {
        let fixture = Fixture::start().await;
        let report = if across {
            let (delivery, terms) =
                across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
            fixture.record_delivery_attempt(
                1,
                20,
                BUY,
                SwapDelivery::Bridge(delivery),
                Some(BridgeOrderTerms::Across(terms)),
            );
            across_report(status, REPORTED_AMOUNT)
        } else {
            record_near_order(&fixture, Address::repeat_byte(0x79));
            format!(r#"{{"status":"{status}","swapDetails":{{"amountOut":"16"}}}}"#)
        };
        let handoff = hinted.then_some(crate::SwapHandoffHint {
            block: BlockNumHash::new(15, B256::repeat_byte(15)),
            deposit_id: Some(U256::from(42)),
            sent: U256::from(BUY_AMOUNT - 99),
        });
        let (url, requests, stub) =
            super::swap_order::spawn_bridge_stub(move |_| report.clone()).await;
        let clients = bridge_clients(&fixture, &url);
        let before = fixture.record();
        assert_eq!(
            fixture
                .owner
                .swap_bridge_report(
                    fixture.operation,
                    uid(1, 20),
                    handoff,
                    &clients,
                    None,
                    crate::OperationHttpClient::direct_for_tests()
                )
                .await
                .unwrap(),
            expected,
            "{status}"
        );
        assert_eq!(fixture.record(), before, "{status}");
        assert_eq!(
            requests.lock().unwrap().len(),
            usize::from(!across || hinted),
            "{status}"
        );
        stub.abort();
        fixture.finish().await;
    }
}

// The fill block Across names is read on the destination chain before it is final there, and
// the report says what the verification would later find in the same receipt.
#[tokio::test]
async fn a_private_across_report_says_what_the_unconfirmed_fill_block_shows_and_records_nothing() {
    use crate::SwapDeliverySeen::{Held, Shielded};
    // Whether the fill's receipt holds the shield, and whether the endpoint serves whole-block
    // receipts.
    for (shield, served) in [(true, true), (false, true), (true, false)] {
        let fixture = Fixture::start().await;
        // Block 30 is the head, above finality depth 1.
        let (destination, config, server) = across_destination(42161).await;
        let profile = config.bridge_profile().unwrap();
        let (spoke_pool, handler) = (profile.spoke_pool(), profile.multicall_handler());
        let railgun = config.require_railgun().unwrap().deployment.contract;
        let (delivery, terms) = private_across_order(
            fixture.config.bridge_profile().unwrap().spoke_pool(),
            handler,
        );
        fixture.record_delivery_attempt(
            1,
            20,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(terms)),
        );
        let received = terms.output_amount + U256::from(7);
        let transfer = |from: Address, to: Address, value: U256| {
            (
                terms.output_token,
                Transfer { from, to, value }.encode_log_data(),
            )
        };
        let mut logs = vec![
            (
                spoke_pool,
                filled_relay(delivery, &terms, 42).encode_log_data(),
            ),
            transfer(handler, delivery.receiver, received),
        ];
        if shield {
            logs.push(transfer(
                delivery.receiver,
                railgun,
                received - U256::from(25),
            ));
        }
        {
            let mut chain = destination.lock().unwrap();
            chain.add_addressed_transaction(30, spoke_pool, Bytes::new(), logs);
            if !served {
                chain.receipt_error = Some(-32601);
            }
        }
        let report = across_report("filled", REPORTED_AMOUNT);
        let (url, _, stub) = super::swap_order::spawn_bridge_stub(move |_| report.clone()).await;
        let clients = bridge_clients(&fixture, &url);
        let handoff = Some(crate::SwapHandoffHint {
            block: BlockNumHash::new(15, B256::repeat_byte(15)),
            deposit_id: Some(U256::from(42)),
            sent: U256::from(BUY_AMOUNT - 99),
        });
        let before = fixture.record();
        let seen = match (served, shield) {
            (false, _) => None,
            (true, true) => Some(Shielded { amount: received }),
            (true, false) => Some(Held { amount: received }),
        };
        assert_eq!(
            fixture
                .owner
                .swap_bridge_report(
                    fixture.operation,
                    uid(1, 20),
                    handoff,
                    &clients,
                    Some(&config),
                    crate::OperationHttpClient::direct_for_tests()
                )
                .await
                .unwrap(),
            crate::SwapBridgeReport::Delivered {
                amount_out: Some(U256::from(REPORTED_AMOUNT)),
                seen,
                transaction: Some(REPORTED_FILL),
            },
            "shield {shield}, served {served}"
        );
        assert_eq!(fixture.record(), before);
        stub.abort();
        server.abort();
        fixture.finish().await;
    }
}

// Each NEAR Intents attempt signs its own deposit address. The trade is the expired attempt's,
// so tracking and the outcome follow its address and signed quote, not the newer attempt's.
#[tokio::test]
async fn near_intents_tracking_follows_the_traded_attempts_deposit_address() {
    let destination_tx = B256::repeat_byte(2);
    let bnb =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(56)
            .cloned()
            .unwrap();
    let fixture = Fixture::start().await;
    let delivery = SwapDelivery::Bridge(BridgeDelivery {
        provider: BridgeProvider::NearIntents,
        destination_chain: 56,
        receiver: Address::repeat_byte(0x77),
        destination_token: Address::ZERO,
        surplus: BridgeSurplus::BridgedByProvider,
        private: None,
    });
    let (older_address, newer_address) = (Address::repeat_byte(0x79), Address::repeat_byte(0x69));
    let older = BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
        deposit_address: older_address,
        min_amount_out: U256::from(15),
        amount_out: U256::from(16),
        deadline: "2026-09-30T01:00:00.000Z".into(),
        signed_quote: r#"{"quote":"older"}"#.into(),
    });
    let newer = BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
        deposit_address: newer_address,
        min_amount_out: U256::from(17),
        amount_out: U256::from(18),
        deadline: "2026-09-30T02:00:00.000Z".into(),
        signed_quote: r#"{"quote":"newer"}"#.into(),
    });
    let (traded_uid, newer_uid) = (uid(1, 20), uid(2, 40));

    // The first attempt expires unfilled, and the retry signs another deposit address.
    fixture.record_delivery_attempt(1, 20, BUY, delivery.clone(), Some(older.clone()));
    let record = fixture.observe(23, 21).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    fixture.record_delivery_attempt(2, 40, BUY, delivery, Some(newer));

    // A finalized settlement then holds the first attempt's trade, which pays its address.
    let logs = vec![
        (fixture.settlement, trade_log(traded_uid)),
        (
            BUY,
            Transfer {
                from: fixture.settlement,
                to: older_address,
                value: U256::from(BUY_AMOUNT),
            }
            .encode_log_data(),
        ),
    ];
    let traded = settle_retry(&fixture, traded_uid, 30, logs).await.unwrap();

    // The uids of the attempt that traded for the first attempt's delivery, and of the
    // attempt that delivered its trade.
    let pairing = |record: &ExecutorRecord| {
        let swap = record.swap().unwrap();
        let first = &swap.orders()[0];
        (
            swap.trade_order(first).map(SwapOrderRecord::uid),
            swap.delivery_order(first).map(SwapOrderRecord::uid),
        )
    };
    let outcomes = |record: &ExecutorRecord| {
        let orders = record.swap().unwrap().orders();
        (
            orders[0].observations().bridge_outcome,
            orders[1].observations().bridge_outcome,
        )
    };
    let record = fixture.record();
    let orders = record.swap().unwrap().orders();
    let (first, retry) = (orders[0].observations(), orders[1].observations());
    assert_eq!(
        (first.traded, first.bridge_handoff, first.delivered),
        (
            Some(traded),
            Some(SwapBridgeHandoff {
                observation: traded,
                deposit_id: None,
            }),
            Some(traded)
        )
    );
    assert_eq!(
        (retry.traded, retry.bridge_handoff, retry.delivered),
        (None, None, None)
    );
    assert_eq!(pairing(&record), (Some(traded_uid), Some(traded_uid)));
    let tracked = record.swap_bridges_to_track().collect::<Vec<_>>();
    assert_eq!(
        tracked.iter().map(|order| order.uid()).collect::<Vec<_>>(),
        vec![traded_uid]
    );
    // The tracked order carries the first attempt's address and signed quote.
    assert_eq!(tracked[0].bridge(), Some(&older));

    // 1Click reports the first attempt's deposit delivered and the retry's address refunded.
    let (asked_older, asked_newer) = (older_address.to_string(), newer_address.to_string());
    let (answers_older, answers_newer) = (asked_older.clone(), asked_newer.clone());
    let (url, requests, stub) = super::swap_order::spawn_bridge_stub_with(move |path, _| {
        if path.contains(&answers_older) {
            (
                200,
                format!(
                    r#"{{"status":"SUCCESS","swapDetails":{{"amountOut":"16","destinationChainTxHashes":[{{"hash":"{destination_tx}","explorerUrl":"https://bscscan.com/tx/{destination_tx}"}}]}}}}"#
                ),
            )
        } else if path.contains(&answers_newer) {
            (200, r#"{"status":"REFUNDED"}"#.to_owned())
        } else {
            (400, "{}".to_owned())
        }
    })
    .await;
    let clients = bridge_clients(&fixture, &url);
    let expected = Some(SwapBridgeOutcome::DeliveredReported {
        amount_out: Some(U256::from(16)),
        transaction_hash: Some(destination_tx),
        destination_transaction: None,
    });
    assert_eq!(observe_bridge(&fixture, &clients, &bnb).await, expected);
    assert_eq!(outcomes(&fixture.record()), (expected.clone(), None));
    // Only the first attempt's address was asked about.
    let only_older_asked = || {
        let requests = requests.lock().unwrap();
        requests.len() == 1
            && requests
                .iter()
                .all(|(path, _)| path.contains(&asked_older) && !path.contains(&asked_newer))
    };
    assert!(only_older_asked());

    // The retry holds no hand-off, so its status is neither recorded nor asked for.
    assert!(matches!(
        fixture.store.record_swap_bridge_outcome(
            fixture.operation,
            newer_uid,
            SwapBridgeOutcome::Refunding
        ),
        Err(ExecutorStoreError::InvalidRecord)
    ));
    assert_eq!(
        fixture
            .owner
            .observe_swap_bridge(
                fixture.operation,
                newer_uid,
                &clients,
                Some(&bnb),
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .unwrap(),
        None
    );
    assert!(only_older_asked());

    // A restart keeps the pairing and the outcome, and asks 1Click nothing more.
    let restarted = restarted_owner(&fixture);
    let record = ExecutorStore::new(fixture.db.clone(), fixture.view.clone(), 1)
        .unwrap()
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == fixture.operation)
        .unwrap();
    assert_eq!(pairing(&record), (Some(traded_uid), Some(traded_uid)));
    assert_eq!(outcomes(&record), (expected.clone(), None));
    assert_eq!(record.swap_bridges_to_track().count(), 0);
    assert_eq!(
        swap_attempt_state(&record, &record.swap().unwrap().orders()[0]),
        SwapOrderState::Done
    );
    for attempt in [traded_uid, newer_uid] {
        assert_eq!(
            restarted
                .observe_swap_bridge(
                    fixture.operation,
                    attempt,
                    &clients,
                    Some(&bnb),
                    crate::OperationHttpClient::direct_for_tests()
                )
                .await
                .unwrap(),
            if attempt == traded_uid {
                expected.clone()
            } else {
                None
            }
        );
    }
    assert!(only_older_asked());
    restarted.shutdown().await;
    stub.abort();
    fixture.finish().await;
}

/// Routine bridge tracking of `order` by `owner`.
async fn track_bridge(
    owner: &ExecutorOwner,
    operation: ExecutorOperationId,
    order: OrderUid,
    clients: &crate::SwapBridgeClients,
    destination: &crate::settings::EffectiveChainConfig,
) -> Option<SwapBridgeOutcome> {
    owner
        .observe_swap_bridge(
            operation,
            order,
            clients,
            Some(destination),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
}

// An open order and its speed-up each sign their own NEAR Intents deposit address and quote.
// Whichever fills, the hand-off, the provider tracking and the outcome use the filled order's
// address and terms, and 1Click's report about the other address records nothing.
#[tokio::test]
async fn near_intents_tracking_follows_whichever_live_order_fills() {
    let destination_tx = B256::repeat_byte(2);
    let bnb =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(56)
            .cloned()
            .unwrap();
    let delivery = SwapDelivery::Bridge(BridgeDelivery {
        provider: BridgeProvider::NearIntents,
        destination_chain: 56,
        receiver: Address::repeat_byte(0x77),
        destination_token: Address::ZERO,
        surplus: BridgeSurplus::BridgedByProvider,
        private: None,
    });
    let addresses = [Address::repeat_byte(0x79), Address::repeat_byte(0x69)];
    let amounts_out = [16_u64, 18];
    let terms = |index: usize| {
        BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
            deposit_address: addresses[index],
            min_amount_out: U256::from(amounts_out[index] - 1),
            amount_out: U256::from(amounts_out[index]),
            deadline: format!("2026-09-30T0{}:00:00.000Z", index + 1),
            signed_quote: format!(r#"{{"quote":"{index}"}}"#),
        })
    };
    let uids = [uid(1, 100), uid(2, 110)];
    for filled in [0_usize, 1] {
        let unfilled = 1 - filled;
        let fixture = Fixture::start().await;
        fixture.record_delivery_attempt(1, 100, BUY, delivery.clone(), Some(terms(0)));
        record_bridge_speed_up(&fixture, 2, 110, uids[0], terms(1));

        // A finalized settlement holds the filled order's trade, which pays its address.
        let logs = vec![
            (fixture.settlement, trade_log(uids[filled])),
            (
                BUY,
                Transfer {
                    from: fixture.settlement,
                    to: addresses[filled],
                    value: U256::from(BUY_AMOUNT),
                }
                .encode_log_data(),
            ),
        ];
        let traded = settle_retry(&fixture, uids[filled], 30, logs)
            .await
            .unwrap();
        let outcomes = |record: &ExecutorRecord| {
            let orders = record.swap().unwrap().orders();
            (
                orders[filled].observations().bridge_outcome,
                orders[unfilled].observations().bridge_outcome,
            )
        };
        let record = fixture.record();
        let orders = record.swap().unwrap().orders();
        let (won, other) = (
            orders[filled].observations(),
            orders[unfilled].observations(),
        );
        assert_eq!(
            (won.traded, won.bridge_handoff, won.delivered),
            (
                Some(traded),
                Some(SwapBridgeHandoff {
                    observation: traded,
                    deposit_id: None,
                }),
                Some(traded)
            ),
            "filled {filled}"
        );
        assert_eq!(
            (other.traded, other.bridge_handoff, other.delivered),
            (None, None, None),
            "filled {filled}"
        );
        let tracked = record.swap_bridges_to_track().collect::<Vec<_>>();
        assert_eq!(
            tracked
                .iter()
                .map(|order| (order.uid(), order.bridge().cloned()))
                .collect::<Vec<_>>(),
            vec![(uids[filled], Some(terms(filled)))],
            "filled {filled}: tracking carries the filled order's address and signed quote"
        );

        // 1Click reports the filled order's deposit delivered and the other address refunded.
        let asked = addresses.map(|address| address.to_string());
        let answers = asked.clone();
        let delivered = amounts_out[filled];
        let (url, requests, stub) = super::swap_order::spawn_bridge_stub_with(move |path, _| {
            if path.contains(&answers[filled]) {
                (
                    200,
                    format!(
                        r#"{{"status":"SUCCESS","swapDetails":{{"amountOut":"{delivered}","destinationChainTxHashes":[{{"hash":"{destination_tx}","explorerUrl":"https://bscscan.com/tx/{destination_tx}"}}]}}}}"#
                    ),
                )
            } else if path.contains(&answers[unfilled]) {
                (200, r#"{"status":"REFUNDED"}"#.to_owned())
            } else {
                (400, "{}".to_owned())
            }
        })
        .await;
        let clients = bridge_clients(&fixture, &url);
        let expected = Some(SwapBridgeOutcome::DeliveredReported {
            amount_out: Some(U256::from(delivered)),
            transaction_hash: Some(destination_tx),
            destination_transaction: None,
        });
        let operation = fixture.operation;
        assert_eq!(
            track_bridge(&fixture.owner, operation, uids[filled], &clients, &bnb).await,
            expected,
            "filled {filled}"
        );
        assert_eq!(
            outcomes(&fixture.record()),
            (expected.clone(), None),
            "filled {filled}"
        );
        let only_filled_asked = || {
            let requests = requests.lock().unwrap();
            requests.len() == 1
                && requests.iter().all(|(path, _)| {
                    path.contains(&asked[filled]) && !path.contains(&asked[unfilled])
                })
        };
        assert!(only_filled_asked(), "filled {filled}");

        // The unfilled order holds no hand-off, so its address's refund or expiry is neither
        // recorded nor asked for, and can't replace the outcome.
        assert!(
            matches!(
                fixture.store.record_swap_bridge_outcome(
                    fixture.operation,
                    uids[unfilled],
                    SwapBridgeOutcome::Refunding
                ),
                Err(ExecutorStoreError::InvalidRecord)
            ),
            "filled {filled}"
        );
        assert_eq!(
            track_bridge(&fixture.owner, operation, uids[unfilled], &clients, &bnb).await,
            None,
            "filled {filled}"
        );
        assert!(only_filled_asked(), "filled {filled}");

        // A restart keeps the hand-off and the outcome on the filled order, and asks 1Click
        // nothing more.
        let restarted = restarted_owner(&fixture);
        let record = stored(&fixture);
        assert_eq!(
            outcomes(&record),
            (expected.clone(), None),
            "filled {filled}"
        );
        assert_eq!(record.swap_bridges_to_track().count(), 0, "filled {filled}");
        assert_eq!(
            swap_attempt_state(&record, &record.swap().unwrap().orders()[filled]),
            SwapOrderState::Done,
            "filled {filled}"
        );
        assert_eq!(
            (
                track_bridge(&restarted, operation, uids[filled], &clients, &bnb).await,
                track_bridge(&restarted, operation, uids[unfilled], &clients, &bnb).await
            ),
            (expected, None),
            "filled {filled}"
        );
        assert!(only_filled_asked(), "filled {filled}");
        restarted.shutdown().await;
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
            .observe_swap_settlement(
                fixture.operation,
                uid(1, 20),
                15,
                crate::OperationHttpClient::direct_for_tests(),
            )
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
        .observe_swap_settlement(
            fixture.operation,
            uid(1, 20),
            15,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(
        failed_receipts.load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(state(&fixture.record(), 0), SwapOrderState::Done);
    {
        let chain = fixture.chain.lock().unwrap();
        assert_block_only_requests(&chain.rpc_requests, chain.block(15));
    }
    owner.shutdown().await;
    unsupported_server.abort();
    working_server.abort();
    fixture.finish().await;
}
