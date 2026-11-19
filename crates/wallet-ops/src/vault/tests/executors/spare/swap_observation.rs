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

mod across_fills;
mod across_reports;
mod attempt_handoffs;
mod cancellations;
mod paired_orders;
mod provider_reports;
mod quotes_and_speed_ups;
mod settlement_receipts;
mod setups_and_shields;
