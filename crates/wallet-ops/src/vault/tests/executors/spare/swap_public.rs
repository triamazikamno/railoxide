//! The destination side of a swap paid from a Public account: its claim, the destination
//! account's setup and delegation check, and the delivery signed before the Public account
//! signs anything. Then the Public account's own side: its approvals and deposit on the chain
//! it pays on, each recorded before it is broadcast, and the hand-off read back from that chain.
//! An order's batch and order are signed as typed data and recorded before the orderbook
//! request, which a restart resends unchanged. What became of the order is read from fabricated
//! settlement receipts: its deposit, proceeds its proxy holds, their withdrawal, and its
//! invalidation. After the hand-off the destination chain's owner tracks the fill on its own
//! chain and the refund on the chain the account pays on, also after a restart. An order whose
//! sold token has a permit signs it in place of an approval and carries it as its pre-hook.

use super::swap_observation::{EXECUTOR, MockChain, execute, private_transaction};
use super::swap_order::{
    across_fee_quote, delegate_setup, read_json_body, spawn_bridge_stub, spawn_bridge_stub_with,
    submitted_order,
};
use super::swap_setup::{
    DESTINATION_CHAIN, DESTINATION_TOKEN, USDC, WETH, broadcaster, destination_chain_config,
    password, setup_approval, setup_fee_limit,
};
use super::*;
use crate::bridge::AcrossClient;
use crate::cow::CowOrderbookClient;
use crate::public_wallet::{PublicErc20, VaultedPublicSigner};
use crate::settings::EffectiveChainConfig;
use crate::signer::SoftwareEvmSigner;
use crate::vault::{
    AcrossOrderTerms, PublicAccountScope, PublicSwapApproval, PublicSwapDeposited,
    PublicSwapInclusion, PublicSwapIntent, PublicSwapObservations, PublicSwapOrder, PublicSwapPath,
    PublicSwapProxyHolding, PublicSwapRecord, PublicSwapSignedTransaction, PublicSwapSourceTerms,
    PublicSwapTransactionKind, SwapAccountChoice, SwapAccountRefusal, SwapAccountRole,
    SwapAccountUse, SwapAdmissionEvidence, SwapApprovedAccount, SwapBridgeHandoff,
    SwapBridgeOutcome, SwapDestinationOutcome, SwapObservation, SwapSubmissionStatus, SwapUseId,
    SwapUseRole, swap_account_refusal,
};
use crate::{
    ExecutorPrivateFeeLimitExceeded, OperationHttpClient, OperationNetworkIsolation,
    PUBLIC_PROXY_DEPLOYING_WITHDRAWAL_GAS_UNITS, PublicActionGasFeeSelection,
    PublicActionProgressUpdate, PublicSwapApprovalsOutcome, PublicSwapDelivery,
    PublicSwapDeliverySigning, PublicSwapOrderOutcome, PublicSwapOrderState, PublicSwapProgress,
    PublicSwapTracking, PublicSwapTransactionOutcome, PublicSwapUseClaim, SwapPairSide,
    SwapReviewChange, SwapSetupStatus, WalletNetworkMode, new_public_swap_batch_nonce,
    public_swap_batch_terms, public_swap_gas_plan, public_swap_order_state, swap_setup_status,
};
use alloy::consensus::{Transaction as _, TxEnvelope};
use alloy::eips::Decodable2718 as _;
use alloy::primitives::{Signature, keccak256};
use alloy::sol_types::{Eip712Domain, SolEvent as _, SolStruct as _, SolValue as _};
use broadcaster_core::contracts::across::{
    MulticallHandler, SpokePool, V3RelayExecutionEventInfo, address_to_bytes32,
    private_delivery_message,
};
use broadcaster_core::contracts::cow::{
    AppData, ORDER_KIND_SELL, OrderUid, invalidate_order_calldata, order_digest, order_uid,
    recover_order_signer,
};
use broadcaster_core::contracts::cow_shed::{
    COWShedFactory, ExecuteHooks, decode_deposit_hook_calls, decode_withdrawal_calls,
    execute_hooks_digest, proxy_address,
};
use broadcaster_core::contracts::erc20_permit::{IERC20Permit, Permit};
use broadcaster_core::contracts::executor::AcrossPrivateDelivery;
use broadcaster_core::contracts::railgun::transferCall;
use broadcaster_core::contracts::shield::build_approve_calldata;
use broadcaster_core::contracts::swap_math::{SWAP_MATH_ADDRESS, SWAP_MATH_CREATION_CODE};

// The settlement's events, as the origin chain's fabricated receipts carry them, and its read
// of an order's fill.
alloy::sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Trade(address indexed owner, address sellToken, address buyToken, uint256 sellAmount, uint256 buyAmount, uint256 feeAmount, bytes orderUid);
    function filledAmount(bytes orderUid) external view returns (uint256);
}

/// The claim of a direct deposit of `bridged_token` from a Public account on `origin_chain`,
/// delivered as at least 1,000 of `destination_token` to the new account `operation`. A failed
/// shield keeps the tokens in that account.
fn claim(
    id: SwapUseId,
    operation: ExecutorOperationId,
    origin_chain: u64,
    bridged_token: Address,
    destination_token: Address,
) -> PublicSwapUseClaim {
    let mut bounds = setup_approval(bridged_token, WETH, SwapDelivery::Reshield).bounds;
    bounds.destination_minimum = Some(U256::from(1_000));
    bounds.destination_shield_fee_bps = Some(crate::RAILGUN_PROTOCOL_FEE_BPS);
    bounds.destination_setup_fee = Some(setup_fee_limit(1_000));
    // A Public account's order has a pre-hook only when it carries a permit.
    bounds.pre_hook_gas_limit = 0;
    PublicSwapUseClaim {
        id,
        origin_chain,
        source: Address::repeat_byte(0x50),
        source_scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: TEST_WALLET_ID.to_owned(),
        },
        account: SwapAccountChoice::New(operation),
        destination_token,
        intent: PublicSwapIntent {
            bridged_token,
            order: false,
        },
        approval: PublicSwapApproval {
            bounds,
            price_verified: Some(true),
            price_acknowledged: false,
            sell_token: bridged_token,
            on_shield_failure: BridgeShieldFailure::KeepOnDestination,
            destination: SwapApprovedAccount {
                address: None,
                setup: true,
            },
            max_gas_cost: U256::from(1_000_000),
        },
    }
}

/// The destination account's record as the store holds it, with the swap its use delivers,
/// the shields issued for that use and what became of them.
struct Saved {
    record: ExecutorRecord,
    swap: PublicSwapRecord,
    shields: Vec<B256>,
    outcome: Option<SwapDestinationOutcome>,
}

fn saved(store: &ExecutorStore, operation: ExecutorOperationId, id: SwapUseId) -> Saved {
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    let SwapUseRole::PublicSourceDestination {
        shields,
        outcome,
        swap,
        ..
    } = record.swap_use(id).unwrap().role()
    else {
        panic!("the use delivers a swap paid from a Public account");
    };
    let (swap, shields, outcome) = ((**swap).clone(), shields.clone(), *outcome);
    Saved {
        record,
        swap,
        shields,
        outcome,
    }
}

/// The current time, Unix seconds.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The deposit id the origin chain's pool gives every deposit.
const DEPOSIT_ID: u64 = 77;
/// The block the origin chain starts at.
const ORIGIN_HEAD: u64 = 20;
/// The fee the Public account's transactions were reviewed at.
const REVIEWED_FEE: PublicActionGasFeeSelection = PublicActionGasFeeSelection::Custom {
    max_fee_per_gas: 100,
    max_priority_fee_per_gas: 1,
};

/// The token an order sells on the origin chain.
const ORDER_SELL_TOKEN: Address = Address::repeat_byte(0x5e);
/// The hook gas limit an order's approval binds.
const ORDER_HOOK_GAS_LIMIT: u64 = 517_000;
/// The id of the quote an order was reviewed with.
const ORDER_QUOTE_ID: i64 = 42;
/// The gas limit the permit pre-hook of an order's approval declares.
const PERMIT_HOOK_GAS_LIMIT: u64 = crate::cow::hook_gas_limit(crate::cow::PUBLIC_PERMIT_HOOK_GAS);
/// The Public account's permit nonce on the token an order sells.
const PERMIT_NONCE: U256 = U256::from_limbs([4, 0, 0, 0]);
/// A JSON-RPC error for an `eth_call` that reverted without data.
const REVERTED: &str = r#"{"code": 3, "message": "execution reverted", "data": "0x"}"#;

/// The typed-data domain of the token an order sells, which it reports through `name()`,
/// `version()` and `DOMAIN_SEPARATOR()`.
fn permit_domain() -> Eip712Domain {
    Eip712Domain::new(
        Some("Token".into()),
        Some("2".into()),
        Some(U256::from(DESTINATION_CHAIN)),
        Some(ORDER_SELL_TOKEN),
        None,
    )
}

/// The math contract's runtime code: what its creation code returns after the 30-byte
/// constructor.
fn swap_math_runtime_code() -> Bytes {
    Bytes::from_static(&SWAP_MATH_CREATION_CODE[30..])
}

/// Each order request an orderbook stub received: the swap's order as the record held it when
/// the request arrived, and the request's body.
type OrderRequests = Arc<Mutex<Vec<(Option<PublicSwapOrder>, Value)>>>;

/// The chain a Public account pays on: a `MockChain` that also answers what the account's
/// transaction step asks, and includes each broadcast transaction in the next block. An
/// approval sets the allowance it names, and a deposit emits the pool's event unless deposits
/// revert. A read of the settlement's fill of an order is the mock chain's. The account's
/// nonce at a block named by its hash counts its transactions canonical blocks hold up to it.
struct OriginChain {
    chain: MockChain,
    source: Address,
    spoke_pool: Address,
    allowance: U256,
    deposits_revert: bool,
    /// What every balance read answers. `None` fails the read.
    balance: Option<U256>,
    /// Addresses without code. Every other address has some.
    codeless: Vec<Address>,
    /// The code at the math contract's address, which an order's post-hook calls.
    math_code: Bytes,
    /// The logs of the next broadcast transaction that isn't a deposit.
    receipt_logs: Vec<(Address, alloy::primitives::LogData)>,
    /// A block number and a timestamp: blocks from that number on are stamped from that time,
    /// one second apart. Earlier blocks keep the mock chain's own time, long past.
    late_from: Option<(u64, u64)>,
    /// Every request served, in order.
    requests: Vec<Value>,
    /// The token, owner and spender of every allowance read.
    allowance_reads: Vec<(Address, Address, Address)>,
    /// The token and account of every balance read.
    balance_reads: Vec<(Address, Address)>,
    /// The sold token's permit: its typed-data domain and the account's nonce. Without one
    /// every read of it reverts.
    permit: Option<(Eip712Domain, U256)>,
    /// Whether a read of the permit sent by itself fails without reverting.
    permit_reads_fail: bool,
    /// Whether a call of a signed permit reverts.
    permit_reverts: bool,
    /// The calldata of every allowance and permit read, grouped by the request that carried
    /// it: the members of an aggregated request, or the one call of a direct one.
    token_reads: Vec<Vec<Bytes>>,
    /// Every signed permit called by itself.
    simulated: Vec<Bytes>,
    /// Every transaction broadcast, in order.
    broadcasts: Vec<(B256, TxEnvelope)>,
    /// Whether every raw send is refused, and nothing is included.
    rejects_sends: bool,
    /// Every raw transaction sent, refused or not, in order. A transaction sent again after
    /// its broadcast is known and not included again.
    sent: Vec<Bytes>,
    /// A block the chain reorganizes from once it serves a nonce read by block hash.
    reorg_on_nonce_read: Option<u64>,
    /// Called with a transaction's hash when its broadcast arrives, before it is included.
    on_broadcast: Box<dyn FnMut(B256) + Send>,
}

impl OriginChain {
    fn methods(&self) -> impl Iterator<Item = &str> {
        self.requests
            .iter()
            .map(|request| request["method"].as_str().unwrap())
    }

    /// What the sold token answers a read of the allowance or of its permit. `None` reverts.
    fn token_read(&mut self, token: Address, input: &[u8]) -> Option<Bytes> {
        if let Ok(read) = PublicErc20::allowanceCall::abi_decode(input) {
            self.allowance_reads.push((token, read.owner, read.spender));
            return Some(B256::from(self.allowance).to_vec().into());
        }
        let (domain, nonce) = self.permit.as_ref()?;
        if input.starts_with(&IERC20Permit::noncesCall::SELECTOR) {
            Some(B256::from(*nonce).to_vec().into())
        } else if input.starts_with(&IERC20Permit::DOMAIN_SEPARATORCall::SELECTOR) {
            Some(domain.separator().to_vec().into())
        } else if input.starts_with(&IERC20Permit::nameCall::SELECTOR) {
            Some(
                IERC20Permit::nameCall::abi_encode_returns(&domain.name.as_ref()?.to_string())
                    .into(),
            )
        } else if input.starts_with(&IERC20Permit::versionCall::SELECTOR) {
            Some(
                IERC20Permit::versionCall::abi_encode_returns(
                    &domain.version.as_ref()?.to_string(),
                )
                .into(),
            )
        } else {
            // The token has no `eip712Domain()`.
            None
        }
    }

    fn respond(&mut self, request: &Value) -> Value {
        self.requests.push(request.clone());
        let params = &request["params"];
        let result = match request["method"].as_str().unwrap() {
            "eth_gasPrice" => json!("0x64"),
            "eth_maxPriorityFeePerGas" => json!("0x1"),
            "eth_feeHistory" => {
                return json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                    "code": -32601, "message": "fee history is unsupported"
                }});
            }
            "eth_getTransactionCount" if params[1].get("blockHash").is_some() => {
                let Some(count) = self.chain.transaction_count_at(self.source, &params[1]) else {
                    return json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                        "code": -32000, "message": "the block isn't canonical"
                    }});
                };
                if let Some(from) = self.reorg_on_nonce_read.take() {
                    self.chain.reorg(from);
                }
                json!(format!("0x{count:x}"))
            }
            "eth_getTransactionCount" => {
                // The head's count also takes in the account's transactions a test included.
                let head = json!(self.chain.block(self.chain.head).hash);
                let included = self.chain.transaction_count_at(self.source, &head).unwrap();
                json!(format!(
                    "0x{:x}",
                    included.max(u64::try_from(self.broadcasts.len()).unwrap())
                ))
            }
            "eth_estimateGas" => json!("0xc350"),
            "eth_getBalance" => json!("0xde0b6b3a7640000"),
            "eth_call" => {
                let call = &params[0];
                let input: Bytes = serde_json::from_value(
                    call.get("input").unwrap_or_else(|| &call["data"]).clone(),
                )
                .unwrap();
                let token = serde_json::from_value(call["to"].clone()).unwrap();
                let failure = |error: &str| {
                    json!({"jsonrpc": "2.0", "id": request["id"], "error":
                        serde_json::from_str::<Value>(error).unwrap()})
                };
                let permit_read = [
                    IERC20Permit::eip712DomainCall::SELECTOR,
                    IERC20Permit::nameCall::SELECTOR,
                    IERC20Permit::versionCall::SELECTOR,
                    IERC20Permit::DOMAIN_SEPARATORCall::SELECTOR,
                    IERC20Permit::noncesCall::SELECTOR,
                ]
                .iter()
                .any(|selector| input.starts_with(selector));
                if let Ok(aggregate) = IMulticall3::tryAggregateCall::abi_decode(&input) {
                    self.token_reads.push(
                        aggregate
                            .calls
                            .iter()
                            .map(|call| call.callData.clone())
                            .collect(),
                    );
                    let results: Vec<_> = aggregate
                        .calls
                        .iter()
                        .map(|call| {
                            let answer = self.token_read(call.target, &call.callData);
                            IMulticall3::Result {
                                success: answer.is_some(),
                                returnData: answer.unwrap_or_default(),
                            }
                        })
                        .collect();
                    json!(Bytes::from(
                        IMulticall3::tryAggregateCall::abi_encode_returns(&results)
                    ))
                } else if input.starts_with(&IERC20Permit::permitCall::SELECTOR) {
                    self.simulated.push(input.clone());
                    if self.permit_reverts {
                        return failure(REVERTED);
                    }
                    json!("0x")
                } else if permit_read {
                    self.token_reads.push(vec![input.clone()]);
                    if self.permit_reads_fail {
                        return failure(r#"{"code": -32002, "message": "unavailable"}"#);
                    }
                    match self.token_read(token, &input) {
                        Some(answer) => json!(answer),
                        None => return failure(REVERTED),
                    }
                } else if let Ok(read) = PublicErc20::balanceOfCall::abi_decode(&input) {
                    self.balance_reads.push((token, read.account));
                    let Some(balance) = self.balance else {
                        return json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                            "code": -32000, "message": "the balance is unavailable"
                        }});
                    };
                    serde_json::to_value(B256::from(balance)).unwrap()
                } else if let Ok(read) = PublicErc20::allowanceCall::abi_decode(&input) {
                    self.token_reads.push(vec![input.clone()]);
                    self.allowance_reads.push((token, read.owner, read.spender));
                    serde_json::to_value(B256::from(self.allowance)).unwrap()
                } else if input.starts_with(&filledAmountCall::SELECTOR) && params[1] == "latest" {
                    // A permit order's tracking reads its fill at the latest block by its tag.
                    let mut at_head = request.clone();
                    at_head["params"][1] = json!(self.chain.block(self.chain.head).hash);
                    return self.chain.respond(&at_head);
                } else {
                    return self.chain.respond(request);
                }
            }
            "eth_sendRawTransaction" => {
                let raw: Bytes = serde_json::from_value(params[0].clone()).unwrap();
                let hash = keccak256(&raw);
                self.sent.push(raw.clone());
                (self.on_broadcast)(hash);
                let refused = if self.rejects_sends {
                    Some("the transaction was refused")
                } else if self.broadcasts.iter().any(|(known, _)| *known == hash) {
                    Some("already known")
                } else {
                    None
                };
                if let Some(message) = refused {
                    return json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                        "code": -32000, "message": message
                    }});
                }
                let transaction = TxEnvelope::decode_2718(&mut &raw[..]).unwrap();
                let number = self.chain.head + 1;
                self.chain.head = number;
                let (status, logs) = if transaction.to() == Some(self.spoke_pool) {
                    let deposit =
                        SpokePool::depositV3Call::abi_decode(transaction.input()).unwrap();
                    let event = SpokePool::FundsDeposited {
                        inputToken: address_to_bytes32(deposit.inputToken),
                        outputToken: address_to_bytes32(deposit.outputToken),
                        inputAmount: deposit.inputAmount,
                        outputAmount: deposit.outputAmount,
                        destinationChainId: deposit.destinationChainId,
                        depositId: U256::from(DEPOSIT_ID),
                        quoteTimestamp: deposit.quoteTimestamp,
                        fillDeadline: deposit.fillDeadline,
                        exclusivityDeadline: 0,
                        depositor: address_to_bytes32(deposit.depositor),
                        recipient: address_to_bytes32(deposit.recipient),
                        exclusiveRelayer: address_to_bytes32(deposit.exclusiveRelayer),
                        message: deposit.message,
                    };
                    if self.deposits_revert {
                        (false, Vec::new())
                    } else {
                        (true, vec![(self.spoke_pool, event.encode_log_data())])
                    }
                } else {
                    // `approve(spender, amount)`: the amount is the second argument. Any other
                    // transaction succeeds and changes nothing here.
                    let approve = build_approve_calldata(Address::ZERO, U256::ZERO);
                    if transaction.input().get(..4) == Some(&approve[..4]) {
                        self.allowance = U256::from_be_slice(&transaction.input()[36..68]);
                    }
                    (true, std::mem::take(&mut self.receipt_logs))
                };
                self.chain
                    .include(number, transaction.clone(), self.source, status, logs);
                self.broadcasts.push((hash, transaction));
                json!(hash)
            }
            "eth_getCode"
                if serde_json::from_value::<Address>(params[0].clone()).unwrap()
                    == SWAP_MATH_ADDRESS =>
            {
                json!(self.math_code)
            }
            "eth_getCode"
                if self
                    .codeless
                    .contains(&serde_json::from_value(params[0].clone()).unwrap()) =>
            {
                json!("0x")
            }
            "eth_getBlockByNumber" => {
                // The fee quote reads the latest block by its tag.
                let mut numbered = request.clone();
                if params[0] == "latest" {
                    numbered["params"][0] = json!(format!("0x{:x}", self.chain.head));
                }
                let mut response = self.chain.respond(&numbered);
                let number = response["result"]["number"].as_str().map(|number| {
                    u64::from_str_radix(number.trim_start_matches("0x"), 16).unwrap()
                });
                if let (Some((from, timestamp)), Some(number)) = (self.late_from, number)
                    && number >= from
                {
                    response["result"]["timestamp"] =
                        json!(format!("0x{:x}", timestamp + (number - from)));
                }
                return response;
            }
            _ => return self.chain.respond(request),
        };
        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
    }
}

/// A claimed swap whose Public account deposits on the mock origin chain and whose destination
/// account, on chain 1, is set up and holds the delivery's signed shield.
struct PublicPayment {
    root: std::path::PathBuf,
    db: Arc<DbStore>,
    vault: DesktopVaultStore,
    view: Arc<DesktopViewSession>,
    origin: EffectiveChainConfig,
    destination: EffectiveChainConfig,
    chain: Arc<Mutex<OriginChain>>,
    /// The chain the swap delivers to, where its fill is read.
    delivery_chain: Arc<Mutex<MockChain>>,
    servers: [tokio::task::JoinHandle<()>; 2],
    store: ExecutorStore,
    operation: ExecutorOperationId,
    id: SwapUseId,
    signer: VaultedPublicSigner,
    source: Address,
    spoke_pool: Address,
    sell_amount: U256,
    delivery: AcrossPrivateDelivery,
    terms: AcrossOrderTerms,
    /// The message the delivery's deposit carries, whose hash the terms sign.
    message: Bytes,
    /// Until when an order of the swap is valid, Unix seconds.
    valid_to: u32,
    /// The nonce an order's hook batch is signed under.
    batch_nonce: B256,
    /// The block the orderbook stub reports the order's trade in. `None` reports no trade.
    trade_block: Arc<Mutex<Option<u64>>>,
    /// Answers for the orderbook stub's next reads of the order's trades, taken in order before
    /// it falls back to [`Self::trade_block`].
    trade_answers: Arc<Mutex<std::collections::VecDeque<Option<u64>>>>,
    /// The destination account's recorded shield.
    shield: B256,
    /// The swap's record as each broadcast found it, with the broadcast transaction's hash.
    at_broadcast: Arc<Mutex<Vec<(B256, PublicSwapRecord)>>>,
    /// Notified when a broadcast arrives.
    broadcast: Arc<Notify>,
}

impl PublicPayment {
    /// A swap that sells `sell_token` on the origin chain, where the Public account's
    /// allowance to the pool is `allowance`. Its approval covers one approval and the deposit.
    async fn start(sell_token: Address, allowance: U256) -> (Self, ExecutorOwner) {
        Self::start_on(sell_token, allowance, false, false).await
    }

    /// A swap whose Public account sells another token through an order that buys at least
    /// 1,010 of the bridged token, with a signed delivery quoted for that amount and for
    /// [`Self::valid_to`]. Its approval allows 100 for gas, so its cushion is 20.
    async fn start_order() -> (Self, ExecutorOwner) {
        Self::start_on(ORDER_SELL_TOKEN, U256::MAX, true, false).await
    }

    /// [`Self::start_order`] for a token with a permit and no allowance to the vault relayer.
    /// The order was reviewed with the permit: its approval binds the pre-hook's gas limit
    /// and no gas from the account.
    async fn start_permit_order() -> (Self, ExecutorOwner) {
        Self::start_on(ORDER_SELL_TOKEN, U256::ZERO, true, true).await
    }

    async fn start_on(
        sell_token: Address,
        allowance: U256,
        order: bool,
        permit: bool,
    ) -> (Self, ExecutorOwner) {
        let (root, db, vault) = desktop_store_with_vault();
        let view = Arc::new(import_wallet_with_metadata(
            &vault,
            TEST_WALLET_ID,
            "Wallet",
        ));
        let configs = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap();
        let mut destination = configs.get(1).cloned().unwrap();
        let mut origin = configs.get(DESTINATION_CHAIN).cloned().unwrap();
        let profile = destination.accepted_executor_profile().unwrap();
        let railgun = destination.require_railgun().unwrap().deployment.contract;
        let spoke_pool = origin.bridge_origin_profile().unwrap().spoke_pool();
        let handler = destination.bridge_profile().unwrap().multicall_handler();
        let signer =
            VaultedPublicSigner::Software(SoftwareEvmSigner::from_private_key([7; 32]).unwrap());
        let source = signer.address();

        let mut mock = MockChain::new(DESTINATION_CHAIN, railgun, profile.delegate());
        mock.head = ORIGIN_HEAD;
        let chain = Arc::new(Mutex::new(OriginChain {
            chain: mock,
            source,
            spoke_pool,
            allowance,
            deposits_revert: false,
            balance: Some(U256::ZERO),
            codeless: Vec::new(),
            math_code: swap_math_runtime_code(),
            receipt_logs: Vec::new(),
            late_from: None,
            requests: Vec::new(),
            allowance_reads: Vec::new(),
            balance_reads: Vec::new(),
            permit: permit.then(|| (permit_domain(), PERMIT_NONCE)),
            permit_reads_fail: false,
            permit_reverts: false,
            token_reads: Vec::new(),
            simulated: Vec::new(),
            broadcasts: Vec::new(),
            rejects_sends: false,
            sent: Vec::new(),
            reorg_on_nonce_read: None,
            on_broadcast: Box::new(|_| {}),
        }));
        let served = chain.clone();
        let (origin_endpoint, origin_server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request: Value| served.lock().unwrap().respond(&request)),
            Arc::default(),
            Arc::default(),
        )
        .await;
        let delivery_chain = Arc::new(Mutex::new(MockChain::new(1, railgun, profile.delegate())));
        let delivering = delivery_chain.clone();
        let (destination_endpoint, destination_server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request: Value| delivering.lock().unwrap().respond(&request)),
            Arc::default(),
            Arc::default(),
        )
        .await;
        origin.finality_depth = 1;
        origin.rpc_route = crate::RpcChainRoute::new(DESTINATION_CHAIN, vec![origin_endpoint]);
        destination.finality_depth = 1;
        destination.rpc_route = crate::RpcChainRoute::new(1, vec![destination_endpoint]);

        let owner = ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            destination.clone(),
            HttpContext::direct_for_tests(),
        )
        .unwrap();
        let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
        let (operation, id) = (
            ExecutorOperationId::random().unwrap(),
            SwapUseId::random().unwrap(),
        );
        let mut request = claim(id, operation, DESTINATION_CHAIN, DESTINATION_TOKEN, USDC);
        request.source = source;
        request.approval.sell_token = sell_token;
        request.approval.max_gas_cost = public_swap_gas_plan(&origin, 1, true, 100, 1)
            .unwrap()
            .max_gas_cost;
        if order {
            request.intent.order = true;
            request.approval.bounds.buy_amount = U256::from(1_010);
            request.approval.bounds.gas_allowance = Some(U256::from(100));
            request.approval.bounds.post_hook_gas_limit = Some(ORDER_HOOK_GAS_LIMIT);
        }
        if permit {
            request.approval.bounds.pre_hook_gas_limit = PERMIT_HOOK_GAS_LIMIT;
            request.approval.max_gas_cost = U256::ZERO;
        }
        let sell_amount = request.approval.bounds.sell_amount;
        // A deposit bridges what it sells, an order what it buys.
        let input_amount = if order {
            request.approval.bounds.buy_amount
        } else {
            sell_amount
        };
        owner.claim_public_swap(request).unwrap();
        store.bind_address(operation, EXECUTOR).unwrap();
        store
            .bind_public_swap_destination(operation, id, EXECUTOR)
            .unwrap();
        let _delegated = delegate_setup(&store, operation, profile);

        // The delivery's shield is in the destination account's record, as signing leaves it.
        let shield_multicall = Bytes::from_static(b"signed destination shield");
        let shield = B256::repeat_byte(70);
        store
            .record_swap_destination_shield(
                operation,
                id,
                IssuedExecutorPayload::new(
                    U256::ONE,
                    profile.delegate(),
                    shield,
                    ExecutorPayloadPurpose::SwapDestinationShield,
                    ExecutorPayloadContext::new(
                        shield_multicall.clone(),
                        ExecutorNonceObservation::new(
                            BlockNumHash::new(12, B256::repeat_byte(12)),
                            U256::ONE,
                        ),
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
        let message = private_delivery_message(
            handler,
            USDC,
            EXECUTOR,
            shield_multicall.clone(),
            Some(EXECUTOR),
        );
        let delivery = AcrossPrivateDelivery {
            handler,
            destination_executor: EXECUTOR,
            shield_multicall,
            fallback: Some(EXECUTOR),
        };
        let now = u32::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        // An order's terms are checked against its validity, so they are quoted now.
        let (quote_timestamp, fill_deadline) = if order {
            (now - 5, now + 3 * 60 * 60)
        } else {
            (1_700_000_000, 1_700_021_600)
        };
        let terms = AcrossOrderTerms {
            spoke_pool,
            input_token: DESTINATION_TOKEN,
            output_token: USDC,
            input_amount,
            output_amount: U256::from(1_000),
            quote_timestamp,
            fill_deadline,
            exclusive_relayer: Address::ZERO,
            exclusivity_parameter: 0,
            recipient: Some(handler),
            message_hash: Some(keccak256(&message)),
        };
        let batch_nonce = new_public_swap_batch_nonce().unwrap();

        let at_broadcast = Arc::new(Mutex::new(Vec::new()));
        let broadcast = Arc::new(Notify::new());
        let (seen, arrived) = (at_broadcast.clone(), broadcast.clone());
        let reader = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
        chain.lock().unwrap().on_broadcast = Box::new(move |hash| {
            seen.lock()
                .unwrap()
                .push((hash, saved(&reader, operation, id).swap));
            arrived.notify_one();
        });
        (
            Self {
                root,
                db,
                vault,
                view,
                origin,
                destination,
                chain,
                delivery_chain,
                servers: [origin_server, destination_server],
                store,
                operation,
                id,
                signer,
                source,
                spoke_pool,
                sell_amount,
                delivery,
                terms,
                message,
                valid_to: now + 5 * 60,
                batch_nonce,
                trade_block: Arc::default(),
                trade_answers: Arc::default(),
                shield,
                at_broadcast,
                broadcast,
            },
            owner,
        )
    }

    fn saved(&self) -> Saved {
        saved(&self.store, self.operation, self.id)
    }

    async fn approve(&self, owner: &ExecutorOwner) -> eyre::Result<PublicSwapApprovalsOutcome> {
        owner
            .submit_public_swap_approvals_with_signer(
                self.operation,
                self.id,
                &self.origin,
                &self.signer,
                REVIEWED_FEE,
                crate::OperationHttpClient::direct_for_tests(),
                &mut |_: PublicActionProgressUpdate| {},
            )
            .await
    }

    async fn deposit(&self, owner: &ExecutorOwner) -> eyre::Result<PublicSwapTransactionOutcome> {
        owner
            .submit_public_swap_deposit_with_signer(
                self.operation,
                self.id,
                &self.origin,
                &self.signer,
                REVIEWED_FEE,
                &self.delivery,
                &self.terms,
                crate::OperationHttpClient::direct_for_tests(),
                &mut |_: PublicActionProgressUpdate| {},
            )
            .await
    }

    /// Sign and submit the swap's order for `delivery` and `terms`.
    async fn order_for(
        &self,
        owner: &ExecutorOwner,
        orderbook: &CowOrderbookClient,
        delivery: &AcrossPrivateDelivery,
        terms: &AcrossOrderTerms,
    ) -> eyre::Result<PublicSwapOrderOutcome> {
        owner
            .submit_public_swap_order_with_signer(
                self.operation,
                self.id,
                &self.origin,
                &self.signer,
                orderbook,
                delivery,
                terms,
                self.valid_to,
                self.batch_nonce,
                Some(ORDER_QUOTE_ID),
                false,
            )
            .await
    }

    /// Sign and submit the swap's order for its signed delivery.
    async fn order(
        &self,
        owner: &ExecutorOwner,
        orderbook: &CowOrderbookClient,
    ) -> eyre::Result<PublicSwapOrderOutcome> {
        self.order_for(owner, orderbook, &self.delivery, &self.terms)
            .await
    }

    /// The Public account's cow-shed proxy on the origin chain.
    fn proxy(&self) -> Address {
        let profile = self.origin.public_swap_profile().unwrap();
        proxy_address(
            profile.cow_shed_factory(),
            profile.cow_shed_implementation(),
            self.source,
        )
    }

    /// An orderbook on the origin chain that records each order request with the swap's order
    /// as the record held it when the request arrived. With `lose_first_response` it accepts
    /// the first order without answering, and answers the next as a duplicate. A read of the
    /// order's trades answers the next of [`Self::trade_answers`], or else
    /// [`Self::trade_block`].
    async fn orderbook(
        &self,
        lose_first_response: bool,
    ) -> (
        CowOrderbookClient,
        OrderRequests,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/polygon", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let requests = OrderRequests::default();
        let recorded = requests.clone();
        let reader = ExecutorStore::new(self.db.clone(), self.view.clone(), 1).unwrap();
        let (operation, id) = (self.operation, self.id);
        let settlement = self.settlement();
        let trade_block = self.trade_block.clone();
        let trade_answers = self.trade_answers.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut stream = BufReader::new(stream);
                let mut request_line = String::new();
                if stream.read_line(&mut request_line).await.unwrap() == 0 {
                    continue;
                }
                let Some(body) = read_json_body(&mut stream).await else {
                    continue;
                };
                if request_line.starts_with("GET") {
                    // The only read is of the order's trades, by its UID.
                    assert!(request_line.contains("/api/v1/trades?orderUid=0x"));
                    let queued = trade_answers.lock().unwrap().pop_front();
                    let reported = queued.unwrap_or_else(|| *trade_block.lock().unwrap());
                    let reply = reported
                        .map_or_else(|| json!([]), |block| json!([{"blockNumber": block}]))
                        .to_string();
                    stream
                        .get_mut()
                        .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes())
                        .await
                        .unwrap();
                    continue;
                }
                let owner = body["from"].as_str().unwrap().parse().unwrap();
                let uid = order_uid(
                    &submitted_order(&body),
                    DESTINATION_CHAIN,
                    settlement,
                    owner,
                );
                let persisted = saved(&reader, operation, id).swap.order().cloned();
                recorded.lock().unwrap().push((persisted, body));
                let served = recorded.lock().unwrap().len();
                if lose_first_response && served == 1 {
                    continue; // The server accepted the order, but the response was lost.
                }
                let (status, reply) = if lose_first_response {
                    (
                        "400 Bad Request",
                        json!({"errorType": "DuplicatedOrder", "description": "order already exists"})
                            .to_string(),
                    )
                } else {
                    ("201 Created", json!(uid.0).to_string())
                };
                stream
                    .get_mut()
                    .write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes())
                    .await
                    .unwrap();
            }
        });
        let client = CowOrderbookClient::new(
            OperationHttpClient::for_tests(
                reqwest::Client::new(),
                OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
            ),
            url,
            DESTINATION_CHAIN,
        )
        .unwrap();
        (client, requests, task)
    }

    /// One more block on the origin chain, which makes the one before it final.
    fn mine(&self) {
        self.chain.lock().unwrap().chain.head += 1;
    }

    /// The origin chain's pinned settlement contract.
    fn settlement(&self) -> Address {
        self.origin.public_swap_profile().unwrap().settlement()
    }

    /// The pool's event for a deposit of this swap's signed delivery that put in `input` for
    /// `output`.
    fn deposited(&self, input: U256, output: U256) -> SpokePool::FundsDeposited {
        SpokePool::FundsDeposited {
            inputToken: address_to_bytes32(self.terms.input_token),
            outputToken: address_to_bytes32(self.terms.output_token),
            inputAmount: input,
            outputAmount: output,
            destinationChainId: U256::from(1),
            depositId: U256::from(DEPOSIT_ID),
            quoteTimestamp: self.terms.quote_timestamp,
            fillDeadline: self.terms.fill_deadline,
            exclusivityDeadline: 0,
            depositor: address_to_bytes32(self.source),
            recipient: address_to_bytes32(self.terms.recipient.unwrap()),
            exclusiveRelayer: address_to_bytes32(self.terms.exclusive_relayer),
            message: self.message.clone(),
        }
    }

    /// A transaction to `to` in a new block of the origin chain, whose receipt carries `logs`.
    /// Returns the block's number.
    fn include(&self, to: Address, logs: Vec<(Address, alloy::primitives::LogData)>) -> u64 {
        let mut origin = self.chain.lock().unwrap();
        let number = origin.chain.head + 1;
        origin.chain.head = number;
        origin
            .chain
            .add_addressed_transaction(number, to, Bytes::new(), logs);
        number
    }

    /// The settlement's event for a fill of the order `uid` that buys `paid`.
    fn trade(&self, uid: OrderUid, paid: U256) -> (Address, alloy::primitives::LogData) {
        (
            self.settlement(),
            Trade {
                owner: self.source,
                sellToken: ORDER_SELL_TOKEN,
                buyToken: DESTINATION_TOKEN,
                sellAmount: self.sell_amount,
                buyAmount: paid,
                feeAmount: U256::ZERO,
                orderUid: uid.0.to_vec().into(),
            }
            .encode_log_data(),
        )
    }

    /// A settlement in a new block that fills the order `uid` and pays `paid` of the bought
    /// token to the proxy. Its receipt also holds `deposit`, when the order's hook ran. The
    /// orderbook stub reports the trade from then on. Returns the block's number.
    fn settle(
        &self,
        uid: OrderUid,
        paid: U256,
        deposit: Option<&SpokePool::FundsDeposited>,
    ) -> u64 {
        let settlement = self.settlement();
        let mut logs = vec![
            self.trade(uid, paid),
            (
                DESTINATION_TOKEN,
                Transfer {
                    from: settlement,
                    to: self.proxy(),
                    value: paid,
                }
                .encode_log_data(),
            ),
        ];
        if let Some(deposit) = deposit {
            logs.push((self.spoke_pool, deposit.encode_log_data()));
        }
        let number = self.include(settlement, logs);
        *self.trade_block.lock().unwrap() = Some(number);
        number
    }

    /// Submit the swap's order, and return its UID.
    async fn submitted(&self, owner: &ExecutorOwner, orderbook: &CowOrderbookClient) -> OrderUid {
        let PublicSwapOrderOutcome::Submitted { uid } = self.order(owner, orderbook).await.unwrap()
        else {
            panic!("the order is submitted");
        };
        uid
    }

    /// Read what became of the swap's order, and return the swap as the record then holds it.
    async fn observe(
        &self,
        owner: &ExecutorOwner,
        orderbook: &CowOrderbookClient,
    ) -> PublicSwapRecord {
        owner
            .observe_public_swap_order(
                self.operation,
                self.id,
                &self.origin,
                orderbook,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        self.saved().swap
    }

    /// How many log queries the origin chain has served.
    fn log_queries(&self) -> usize {
        self.chain
            .lock()
            .unwrap()
            .methods()
            .filter(|method| *method == "eth_getLogs")
            .count()
    }

    /// The swap's order state now.
    fn state(&self) -> Option<PublicSwapOrderState> {
        public_swap_order_state(&self.saved().swap)
    }

    /// Deposit from the Public account, and read the hand-off once its block is final.
    async fn handed_off(&self, owner: &ExecutorOwner) -> SwapBridgeHandoff {
        self.deposit(owner).await.unwrap();
        self.mine();
        owner
            .observe_public_swap_handoff(
                self.operation,
                self.id,
                &self.origin,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap()
            .unwrap()
    }

    /// A relayer's fill of the swap's deposit in a new block of the destination chain, which
    /// the block after it makes final: the pinned pool's event for a deposit of `input` for
    /// `output`, the handler's transfer of `output` to the destination account and, with
    /// `shielded`, that account's shield. Returns the block and the fill's transaction.
    fn fill(&self, input: U256, output: U256, shielded: bool) -> (BlockNumHash, B256) {
        let profile = self.destination.bridge_profile().unwrap();
        let (pool, handler) = (profile.spoke_pool(), profile.multicall_handler());
        let railgun = self
            .destination
            .require_railgun()
            .unwrap()
            .deployment
            .contract;
        let recipient = address_to_bytes32(handler);
        let message_hash = self.terms.message_hash.unwrap();
        let fill = SpokePool::FilledRelay {
            inputToken: address_to_bytes32(self.terms.input_token),
            outputToken: address_to_bytes32(self.terms.output_token),
            inputAmount: input,
            outputAmount: output,
            repaymentChainId: U256::ONE,
            originChainId: U256::from(DESTINATION_CHAIN),
            depositId: U256::from(DEPOSIT_ID),
            fillDeadline: self.terms.fill_deadline,
            exclusivityDeadline: 0,
            exclusiveRelayer: B256::ZERO,
            relayer: address_to_bytes32(Address::repeat_byte(0x55)),
            depositor: address_to_bytes32(self.source),
            recipient,
            messageHash: message_hash,
            relayExecutionInfo: V3RelayExecutionEventInfo {
                updatedRecipient: recipient,
                updatedMessageHash: message_hash,
                updatedOutputAmount: output,
                fillType: 0,
            },
        };
        let transfer = |from: Address, to: Address, value: U256| {
            (USDC, Transfer { from, to, value }.encode_log_data())
        };
        let mut logs = vec![
            (pool, fill.encode_log_data()),
            transfer(handler, EXECUTOR, output),
        ];
        if shielded {
            // The shield moves the net amount to Railgun and the fee to its treasury.
            let fee = U256::from(25);
            logs.extend([
                transfer(EXECUTOR, railgun, output - fee),
                transfer(EXECUTOR, Address::repeat_byte(0xfe), fee),
            ]);
        }
        let mut chain = self.delivery_chain.lock().unwrap();
        let number = chain.head + 1;
        chain.add_addressed_transaction(number, pool, Bytes::new(), logs);
        chain.head = number + 1;
        (chain.block(number), chain.transactions.last().unwrap().0)
    }

    /// One routine tracking step of the swap on `owner`, the destination chain's.
    async fn track(
        &self,
        owner: &ExecutorOwner,
        across: &AcrossClient,
        orderbook: Option<&CowOrderbookClient>,
    ) -> eyre::Result<PublicSwapProgress> {
        owner
            .track_public_swap(
                self.operation,
                self.id,
                PublicSwapTracking {
                    origin: &self.origin,
                    across,
                    orderbook,
                    explicit: false,
                    client: crate::OperationHttpClient::direct_for_tests(),
                },
            )
            .await
    }

    /// The swaps `owner` lists for tracking on the destination account.
    fn to_track(&self, owner: &ExecutorOwner) -> Vec<SwapUseId> {
        owner
            .records()
            .unwrap()
            .iter()
            .find(|record| record.operation() == self.operation)
            .unwrap()
            .public_swaps_to_track()
            .map(|(id, _)| id)
            .collect()
    }

    /// No inclusion was looked up by a transaction's hash, then the fixture is torn down.
    async fn finish(self, owner: ExecutorOwner) {
        self.finish_after_lookups(owner, &[]).await;
    }

    /// [`Self::finish`] for a swap whose refund was verified: Across names the transactions
    /// `refunds`, which are its own and are looked up for their blocks.
    async fn finish_after_lookups(self, owner: ExecutorOwner, refunds: &[B256]) {
        assert!(
            self.chain
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter(|request| matches!(
                    request["method"].as_str().unwrap(),
                    "eth_getTransactionReceipt" | "eth_getTransactionByHash"
                ))
                .all(|request| {
                    refunds.contains(&serde_json::from_value(request["params"][0].clone()).unwrap())
                }),
            "a Public account's transaction is never looked up by its hash"
        );
        for server in &self.servers {
            server.abort();
        }
        // The broadcast hook holds a store of its own.
        self.chain.lock().unwrap().on_broadcast = Box::new(|_| {});
        owner.shutdown().await;
        drop(owner);
        let Self {
            root,
            db,
            vault,
            view,
            store,
            ..
        } = self;
        drop(store);
        drop(view);
        drop(vault);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// The gas limit the tests review an invalidation with.
const INVALIDATION_GAS_LIMIT: u64 = 100_000;

/// Across's record of the swap's deposit: its `status`, the block of its fill and the refund
/// transaction it names.
fn across_deposit(status: &str, fill_block: Option<u64>, refund: Option<B256>) -> String {
    json!({"deposit": {
        "status": status,
        "fillBlockNumber": fill_block,
        "depositRefundTxHash": refund,
        "outputAmount": "1",
        "recipient": EXECUTOR,
        "destinationChainId": "1",
    }})
    .to_string()
}

/// The path and body of each request a bridge stub served.
type BridgeLookups = Arc<Mutex<Vec<(String, Value)>>>;

/// An Across API stub that answers every deposit lookup with what the returned reply holds,
/// at first a pending deposit. Returned with the lookups it served.
async fn across_stub() -> (
    AcrossClient,
    Arc<Mutex<String>>,
    BridgeLookups,
    tokio::task::JoinHandle<()>,
) {
    let reply = Arc::new(Mutex::new(across_deposit("pending", None, None)));
    let served = reply.clone();
    let (url, lookups, task) = spawn_bridge_stub(move |_| served.lock().unwrap().clone()).await;
    let client = AcrossClient::new(
        OperationHttpClient::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        ),
        url,
    )
    .unwrap();
    (client, reply, lookups, task)
}

/// A tracking step that found nothing.
const NO_PROGRESS: PublicSwapProgress = PublicSwapProgress {
    changed: false,
    refresh_public_balances: false,
    finished: false,
    permit_used_up: false,
    destination_balance: None,
};

/// How many calls each request of allowance and permit reads carried since the last count.
fn token_reads_served(payment: &PublicPayment) -> Vec<usize> {
    std::mem::take(&mut payment.chain.lock().unwrap().token_reads)
        .iter()
        .map(Vec::len)
        .collect()
}

mod delivery;
mod deposits;
mod orders;
mod permits;
mod review_and_setup;
mod withdrawals_and_invalidations;
