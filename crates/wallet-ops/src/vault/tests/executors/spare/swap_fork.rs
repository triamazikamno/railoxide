//! Opt-in mainnet-fork scenarios for swap outcomes, driven through the wallet's own
//! setup, signing, and observation paths with synthetic Railgun proofs:
//!
//! `ETH_FORK_RPC_URL=<mainnet RPC> cargo test -p wallet-ops swap_fork -- --ignored`
//!
//! Set `ANVIL_BIN` when `anvil` is not on `PATH`.
//!
//! Settlements are sent by an impersonated allow-listed solver. The harness adds
//! `0xdEaD` to the `GPv2` solver allow list through the authenticator's manager,
//! because Railgun accepts a synthetic proof only when `tx.origin` is that
//! `VERIFICATION_BYPASS` address, and a settlement can run the swap's pre-hook.

use super::swap_order::{OutputPois, spawn_bridge_stub, spawn_orderbook, submitted_order};
use super::swap_setup::{USDC, WETH, broadcaster, password, setup_approval};
use super::*;
use crate::cow::{CowOrderbookClient, CowQuote};
use crate::tests::cow_fork::{
    ForkChain, MULTICALL3, RAILGUN, RailgunTree, VERIFICATION_BYPASS, synthetic_transaction,
    unshield_to,
};
use crate::{
    DelegatedSwapExecutor, ExecutorRecoveryExecution, ExecutorRecoveryFunding,
    IssuedExecutorTransaction, OperationHttpClient, OperationNetworkIsolation,
    PreparedExecutorRecovery, SwapAmountPlan, SwapAmountRequest, SwapInputPlan, SwapOrderOutcome,
    SwapOrderState, SwapPrice, SwapSetupStatus, WalletNetworkMode, swap_order_state,
};
use alloy::rpc::types::TransactionReceipt;
use broadcaster_core::contracts::across::{SpokePool, address_to_bytes32};
use broadcaster_core::contracts::cow::{
    AppData, AppDataHooks, BUY_NATIVE_TOKEN, GPv2Settlement, Order, OrderUid,
};
use broadcaster_core::contracts::railgun::{Call, Shield};

alloy::sol! {
    interface ForkSwapSettlement {
        function filledAmount(bytes orderUid) external view returns (uint256);
    }

    interface ForkAllowance {
        function allowance(address owner, address spender) external view returns (uint256);
    }

    interface ForkTransfer {
        function transfer(address to, uint256 amount) external returns (bool);
    }
}

const SELL_AMOUNT: u64 = 10_000_000_000_000_000;

const ARBITRUM_ONE: u64 = 42_161;
const ARBITRUM_USDC: Address =
    alloy::primitives::address!("af88d065e77c8cC2239327C5EDb3A432268e5831");
/// The receiver on Arbitrum One of every Across order here.
const BRIDGE_RECEIVER: Address = Address::repeat_byte(0x77);
/// The approved minimum on Arbitrum One of every Across order here, in USDC base units.
const DESTINATION_MINIMUM: u64 = 15_000_000;
/// What a settlement with surplus pays above the order's `buyAmount`, in USDC base units.
const SURPLUS: u64 = 1_000_000;

/// Across delivery of bought USDC as Arbitrum One USDC, with `surplus` handled as chosen.
const fn across_delivery(surplus: BridgeSurplus) -> SwapDelivery {
    SwapDelivery::Bridge(BridgeDelivery {
        provider: BridgeProvider::Across,
        destination_chain: ARBITRUM_ONE,
        receiver: BRIDGE_RECEIVER,
        destination_token: ARBITRUM_USDC,
        surplus,
    })
}

/// The transaction builder the wallet plans swaps with. Planning never reads its contracts.
const fn builder() -> railgun_wallet::TransactionBuilder {
    railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: Address::repeat_byte(4),
        relay_adapt_contract: Address::repeat_byte(5),
    }
}

/// A published order of a delegated swap executor.
struct Swap {
    operation: ExecutorOperationId,
    executor: Address,
    note: Utxo,
    input: ExecutorInputIdentity,
    order: Order,
    uid: OrderUid,
    signature: Bytes,
    hooks: AppDataHooks,
    /// Length of the signed app data document.
    app_data_len: usize,
}

struct Wallet {
    root: std::path::PathBuf,
    db: Arc<DbStore>,
    vault: DesktopVaultStore,
    view: Arc<DesktopViewSession>,
    chain: crate::settings::EffectiveChainConfig,
    owner: ExecutorOwner,
    positions: AtomicU64,
    /// The buy token and delivery of new swaps. The sell token is WETH.
    pair: (Address, SwapDelivery),
}

impl Wallet {
    fn open(fork: &ForkChain) -> Self {
        let (root, db, vault) = desktop_store_with_vault();
        let view = Arc::new(import_wallet_with_metadata(
            &vault,
            TEST_WALLET_ID,
            "Wallet",
        ));
        let chains = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap();
        let mut chain = chains.get(1).cloned().unwrap();
        chain.rpc_route = crate::RpcChainRoute::new(1, vec![fork.url()]).with_multicall(MULTICALL3);
        let owner = ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            chain.clone(),
            HttpContext::direct_for_tests(),
        )
        .unwrap();
        Self {
            root,
            db,
            vault,
            view,
            chain,
            owner,
            // Leaf positions past a tree's capacity have never been nullified on chain.
            positions: AtomicU64::new(1 << 20),
            pair: (USDC, SwapDelivery::Reshield),
        }
    }

    fn note(&self, tree: RailgunTree, value: U256) -> Utxo {
        Utxo::new(
            broadcaster_core::notes::Note::new_change(
                self.view.scan_keys().master_public_key,
                WETH,
                value,
                [7; 16],
            ),
            u32::from(tree.number),
            self.positions.fetch_add(1, Ordering::Relaxed),
            UtxoSource {
                tx_hash: B256::ZERO,
                block_number: 0,
                block_timestamp: 0,
            },
            UtxoCommitmentKind::Shield,
        )
    }

    fn nullifier(&self, note: &Utxo) -> B256 {
        B256::from(note.nullifier(self.view.scan_keys().nullifying_key))
    }

    /// Delegate a fresh executor with a real delegation-only setup, then sign,
    /// persist, and submit its order to a local orderbook stub.
    async fn swap(&self, fork: &ForkChain) -> Swap {
        let delegated = self.delegate(fork).await;
        let tree = fork.railgun_tree().await;
        let note = self.note(tree, U256::from(SELL_AMOUNT) * U256::from(2));
        let transactions = vec![synthetic_transaction(
            tree,
            self.nullifier(&note),
            delegated.executor(),
            Some(unshield_to(
                delegated.executor(),
                WETH,
                U256::from(SELL_AMOUNT),
            )),
        )];
        self.submit(fork, delegated, note, Some(transactions), None)
            .await
    }

    /// Delegate a fresh executor with a real delegation-only setup.
    async fn delegate(&self, fork: &ForkChain) -> DelegatedSwapExecutor {
        let authorization = password();
        let delegate = self.chain.accepted_executor_profile().unwrap().delegate();
        let operation = ExecutorOperationId::random().unwrap();
        let prepared = self
            .owner
            .prepare_swap_setup(
                operation,
                broadcaster(delegate),
                setup_approval(WETH, self.pair.0, self.pair.1),
                &authorization,
            )
            .await
            .unwrap();
        let executor = prepared.context().executor;
        let tree = fork.railgun_tree().await;
        let fee = self.note(tree, U256::ONE);
        let setup = railgun_wallet::TransactionCall {
            to: executor,
            data: RelayAdapt7702::executeCall {
                _transactions: vec![synthetic_transaction(
                    tree,
                    self.nullifier(&fee),
                    executor,
                    None,
                )],
                _actionData: RelayAdapt7702ActionData {
                    requireSuccess: true,
                    minGasLimit: U256::ZERO,
                    calls: Vec::new(),
                },
                _nonce: U256::ZERO,
                _signature: Bytes::new(),
            }
            .abi_encode()
            .into(),
        };
        let issued = self
            .owner
            .issue_operation(
                &prepared,
                &setup,
                std::slice::from_ref(&fee),
                &authorization,
            )
            .await
            .unwrap();
        // The wallet's type-4 setup, delivered as a broadcaster would.
        let receipt = fork
            .send(
                issued
                    .transaction()
                    .clone()
                    .from(VERIFICATION_BYPASS)
                    .gas_limit(3_000_000),
            )
            .await;
        assert!(receipt.status(), "the setup delegates the executor");
        let setup_block = receipt.block_number.unwrap();
        fork.mine(self.chain.finality_depth).await;
        let SwapSetupStatus::Delegated(delegated) = self
            .owner
            .observe_swap_setup(operation, setup_block..setup_block + 1)
            .await
            .unwrap()
        else {
            panic!("the confirmed setup delegated the executor");
        };
        delegated
    }

    /// The executor at its current confirmed nonce, for a retry after `block`.
    async fn redelegate(
        &self,
        operation: ExecutorOperationId,
        block: u64,
    ) -> DelegatedSwapExecutor {
        let SwapSetupStatus::Delegated(delegated) = self
            .owner
            .observe_swap_setup(operation, block..block + 1)
            .await
            .unwrap()
        else {
            panic!("the swap executor stays delegated");
        };
        delegated
    }

    /// Plan selling `amount` from `notes` in `delegated`'s next order, which invalidates
    /// `invalidates`.
    fn plan(
        &self,
        delegated: DelegatedSwapExecutor,
        notes: &[Utxo],
        amount: U256,
        invalidates: Option<OrderUid>,
    ) -> SwapAmountPlan {
        let swap_profile = self.chain.swap_profile().unwrap();
        crate::plan_swap_inputs(
            &builder(),
            &swap_profile,
            delegated,
            notes,
            &SwapAmountRequest {
                sell_token: WETH,
                buy_token: self.pair.0,
                amount,
                delivery: self.pair.1,
                byte_budget: None,
            },
            swap_profile.app_data_byte_budget(),
            invalidates,
        )
        .unwrap()
    }

    /// Plan `note` with `invalidates`, then sign, persist, and submit the order to a local
    /// orderbook stub. Without `transactions`, a retry reuses the recorded proof.
    async fn submit(
        &self,
        fork: &ForkChain,
        delegated: DelegatedSwapExecutor,
        note: Utxo,
        transactions: Option<Vec<Transaction>>,
        invalidates: Option<OrderUid>,
    ) -> Swap {
        let SwapAmountPlan::Fits(plan) = self.plan(
            delegated,
            std::slice::from_ref(&note),
            U256::from(SELL_AMOUNT),
            invalidates,
        ) else {
            panic!("one note fits one order");
        };
        self.sign(fork, plan, std::slice::from_ref(&note), transactions)
            .await
    }

    /// Sign, persist, and submit `plan`'s order, which spends `notes`, to a local orderbook
    /// stub. A Bridge order is quoted by a local Across stub. Without `transactions`, a retry
    /// reuses the recorded proof.
    async fn sign(
        &self,
        fork: &ForkChain,
        plan: SwapInputPlan,
        notes: &[Utxo],
        transactions: Option<Vec<Transaction>>,
    ) -> Swap {
        let authorization = password();
        let (operation, executor) = (plan.operation().unwrap(), plan.executor());
        let swap_profile = self.chain.swap_profile().unwrap();
        let quote_fee = SELL_AMOUNT / 1_000;
        let buy_token = if self.pair.0 == Address::ZERO {
            BUY_NATIVE_TOKEN
        } else {
            self.pair.0
        };
        let quote: CowQuote = serde_json::from_value(json!({
            "quote": {
                "sellToken": WETH, "buyToken": buy_token,
                "sellAmount": (SELL_AMOUNT - quote_fee).to_string(), "buyAmount": "20000000",
                "validTo": 1, "feeAmount": quote_fee.to_string(), "gasAmount": "0",
                "gasPrice": "0", "sellTokenPrice": "1000000000000", "kind": "sell",
                "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }))
        .unwrap();
        let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
        let mut review = crate::price_swap_review(
            plan,
            quote,
            SwapPrice::Unverified,
            U256::from(25),
            U256::from(25),
            50,
            crate::cow::GAS_SHARE_BALANCED_BPS,
            std::time::Duration::from_mins(10),
            1,
            U256::ZERO,
            isolation,
        )
        .unwrap();
        let bridged = matches!(self.pair.1, SwapDelivery::Bridge(_));
        if bridged {
            review.set_bridge_for_tests(crate::SwapBridgeQuote {
                provider: BridgeProvider::Across,
                destination_minimum: U256::from(DESTINATION_MINIMUM),
                expected_output: U256::from(DESTINATION_MINIMUM),
                fee: Some(U256::ZERO),
                leg: crate::BridgeLegPrice::SameAsset,
                fill_time_sec: None,
            });
        }
        let (orderbook_url, submissions, orderbook_task) = spawn_orderbook(
            self.db.clone(),
            self.view.clone(),
            operation,
            swap_profile.settlement(),
        )
        .await;
        let orderbook = CowOrderbookClient::new(
            OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
            orderbook_url,
            1,
        )
        .unwrap();
        let across = if bridged {
            Some(self.across_stub(fork, &orderbook).await)
        } else {
            None
        };
        let destination = crate::bridge::BridgeDestination {
            destination_token: ARBITRUM_USDC,
            intermediate: USDC,
            symbol: "USDC".to_owned(),
            same_asset: true,
            near: None,
        };
        let route = across
            .as_ref()
            .map(|(clients, arbitrum, _)| crate::SwapBridgeRoute {
                clients,
                destination: &destination,
                destination_chain: arbitrum,
            });
        let transactions = transactions.unwrap_or_else(|| {
            let record = self
                .owner
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == operation)
                .unwrap();
            crate::reusable_swap_proof(&record, review.plan(), notes)
                .expect("a retry for the same notes and amount reuses the proof")
                .0
        });
        let output_pois = OutputPois::default();
        let tokens = crate::settings::EffectiveTokenRegistry {
            tokens: std::collections::BTreeMap::new(),
        };
        let outcome = self
            .owner
            .issue_swap_order(crate::SwapOrderSigning {
                review: &review,
                private_minimum: review.suggested_private_minimum(),
                price_acknowledged: true,
                transactions,
                inputs: notes,
                change_output_pois: Vec::new(),
                output_pois: &output_pois,
                authorization: &authorization,
                orderbook: &orderbook,
                anchor_cache: &crate::TokenAnchorRateCache::new(),
                token_registry: &tokens,
                bridge: route,
                destination_minimum: bridged.then_some(U256::from(DESTINATION_MINIMUM)),
            })
            .await
            .unwrap();
        orderbook_task.abort();
        if let Some((_, _, across_task)) = across {
            across_task.abort();
        }
        let SwapOrderOutcome::Submitted { uid } = outcome else {
            panic!("the order is submitted");
        };
        let (persisted, body) = submissions.lock().unwrap()[0].clone();
        assert!(persisted);
        let app_data = body["appData"].as_str().unwrap();
        Swap {
            operation,
            executor,
            input: ExecutorInputIdentity::from_utxo(&notes[0]),
            note: notes[0].clone(),
            order: submitted_order(&body),
            uid,
            signature: body["signature"].as_str().unwrap().parse().unwrap(),
            hooks: serde_json::from_str::<AppData>(app_data)
                .unwrap()
                .metadata
                .hooks,
            app_data_len: app_data.len(),
        }
    }

    /// Across clients on `orderbook`'s route whose fee quotes come from a local stub, and the
    /// destination chain, Arbitrum One. The quote is dated at the fork's latest block, which
    /// the `SpokePool` checks at the deposit, and its fill deadline is three hours later: past
    /// the order's expiry plus the wallet's margin, and within the `SpokePool`'s buffer.
    async fn across_stub(
        &self,
        fork: &ForkChain,
        orderbook: &CowOrderbookClient,
    ) -> (
        crate::SwapBridgeClients,
        crate::settings::EffectiveChainConfig,
        tokio::task::JoinHandle<()>,
    ) {
        let quoted = fork.timestamp().await;
        let spoke_pool = self.chain.bridge_profile().unwrap().spoke_pool();
        let arbitrum = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(ARBITRUM_ONE)
        .cloned()
        .unwrap();
        let destination_spoke_pool = arbitrum.bridge_profile().unwrap().spoke_pool();
        let (url, _, task) = spawn_bridge_stub(move |_| {
            json!({
                "outputAmount": DESTINATION_MINIMUM.to_string(),
                "totalRelayFee": {"total": "10000"},
                "lpFee": {"total": "0"},
                "timestamp": quoted.to_string(),
                "fillDeadline": (quoted + 3 * 60 * 60).to_string(),
                "exclusiveRelayer": Address::ZERO,
                "exclusivityDeadline": 0,
                "spokePoolAddress": spoke_pool,
                "destinationSpokePoolAddress": destination_spoke_pool,
                "isAmountTooLow": false,
                "limits": {"minDeposit": "1", "maxDeposit": "1000000000000"},
                "estimatedFillTimeSec": 2
            })
            .to_string()
        })
        .await;
        let mut clients = self.owner.swap_bridge_clients(orderbook).unwrap();
        clients.across = crate::bridge::AcrossClient::new(
            OperationHttpClient::for_tests(
                reqwest::Client::new(),
                OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
            ),
            url,
        )
        .unwrap();
        (clients, arbitrum, task)
    }

    /// Synthetic transactions with the shapes of `plan`'s pre-hook, and the notes they spend,
    /// for an order that is signed but never settled. Each of `notes` must be in its own
    /// tree, so each transaction spends one note; one spent whole has no change output.
    fn synthetic_pre_hook(
        &self,
        plan: &SwapInputPlan,
        notes: &[Utxo],
    ) -> (Vec<Utxo>, Vec<Transaction>) {
        let profile = self.chain.swap_profile().unwrap();
        let preview = builder()
            .preview_mixed_private_action_plan(notes, &plan.proof_request(&profile).unwrap())
            .unwrap();
        let selected = notes
            .iter()
            .filter(|note| {
                preview
                    .selected_inputs
                    .iter()
                    .any(|input| (input.tree, input.position) == (note.tree, note.position))
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(selected.len(), preview.transactions.len());
        let transactions = selected
            .iter()
            .zip(&preview.transactions)
            .map(|(note, shape)| {
                assert_eq!((shape.input_count, shape.has_unshield), (1, true));
                let tree = RailgunTree {
                    number: u16::try_from(note.tree).unwrap(),
                    root: B256::ZERO,
                };
                let mut transaction = synthetic_transaction(
                    tree,
                    self.nullifier(note),
                    plan.executor(),
                    Some(unshield_to(plan.executor(), WETH, note.note.value)),
                );
                if shape.output_count == 1 {
                    transaction.commitments.remove(0);
                    transaction.boundParams.commitmentCiphertext.clear();
                }
                transaction
            })
            .collect();
        (selected, transactions)
    }

    fn record(&self, operation: ExecutorOperationId) -> ExecutorRecord {
        self.owner
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    }

    /// Place an Across order with `surplus` handled as chosen from a fresh executor, and
    /// settle it through `GPv2Settlement` with both hooks, paying `SURPLUS` above its limit.
    /// Returns the order, the settlement and the persisted deposit terms, once the
    /// settlement is final and observed.
    async fn settle_across(
        &mut self,
        fork: &ForkChain,
        surplus: BridgeSurplus,
    ) -> (Swap, TransactionReceipt, AcrossOrderTerms) {
        self.pair = (USDC, across_delivery(surplus));
        let swap = self.swap(fork).await;
        let record = self.record(swap.operation);
        let Some(BridgeOrderTerms::Across(terms)) = first_order(&record).bridge().cloned() else {
            panic!("an Across order keeps its deposit terms");
        };
        let receipt = fork
            .settle_with_surplus(
                &swap.order,
                swap.signature.clone(),
                &swap.hooks.pre,
                &swap.hooks.post,
                U256::from(SURPLUS),
            )
            .await;
        assert!(receipt.status(), "the solver settles with both hooks");
        let settled = receipt.block_number.unwrap();
        fork.mine(self.chain.finality_depth).await;
        self.owner
            .observe_swap_settlement(swap.operation, swap.uid, settled)
            .await
            .unwrap();
        (swap, receipt, terms)
    }

    /// Prepare a broadcaster-funded recovery of `amount` of `token`, or an early cancellation
    /// without `token`.
    async fn prepare_recovery(
        &self,
        operation: ExecutorOperationId,
        token: Option<Address>,
        amount: U256,
    ) -> PreparedExecutorRecovery {
        let delegate = self.chain.accepted_executor_profile().unwrap().delegate();
        let prepared = match token {
            Some(token) => {
                self.owner
                    .prepare_recovery(
                        operation,
                        ExecutorAsset::Erc20(token),
                        amount,
                        ExecutorRecoveryFunding::PublicBroadcaster {
                            candidate: Box::new(broadcaster(delegate)),
                            maximum_private_fee: U256::from(1_000),
                        },
                        &password(),
                    )
                    .await
            }
            None => {
                self.owner
                    .prepare_swap_cancellation(
                        operation,
                        broadcaster(delegate),
                        U256::from(1_000),
                        &password(),
                    )
                    .await
            }
        };
        prepared.unwrap()
    }

    /// Sign and persist a prepared paid recovery through the wallet's issuance path, with a
    /// synthetic private fee transaction in place of the broadcaster payment.
    async fn issue_recovery(
        &self,
        fork: &ForkChain,
        prepared: PreparedExecutorRecovery,
    ) -> IssuedExecutorTransaction {
        let ExecutorRecoveryExecution::PaidExecute { nonce } = prepared.execution() else {
            panic!("broadcaster recovery is a paid execute");
        };
        let (executor, calls) = (prepared.source(), prepared.calls().to_vec());
        let preparation = self
            .owner
            .prepare_broadcaster_recovery_execution(Arc::new(prepared))
            .unwrap();
        let tree = fork.railgun_tree().await;
        let fee = self.note(tree, U256::ONE);
        let call = railgun_wallet::TransactionCall {
            to: executor,
            data: RelayAdapt7702::executeCall {
                _transactions: vec![synthetic_transaction(
                    tree,
                    self.nullifier(&fee),
                    executor,
                    None,
                )],
                _actionData: RelayAdapt7702ActionData {
                    requireSuccess: true,
                    minGasLimit: U256::ZERO,
                    calls,
                },
                _nonce: nonce,
                _signature: Bytes::new(),
            }
            .abi_encode()
            .into(),
        };
        self.owner
            .issue_operation(&preparation, &call, std::slice::from_ref(&fee), &password())
            .await
            .unwrap()
    }

    async fn observe(
        &self,
        operation: ExecutorOperationId,
        blocks: std::ops::Range<u64>,
    ) -> ExecutorRecord {
        self.owner
            .observe_swap(operation, blocks)
            .await
            .unwrap()
            .record()
            .clone()
    }

    async fn finish(self) {
        self.owner.shutdown().await;
        drop(self.owner);
        drop(self.view);
        drop(self.vault);
        drop(self.db);
        std::fs::remove_dir_all(self.root).unwrap();
    }
}

fn first_order(record: &ExecutorRecord) -> &SwapOrderRecord {
    &record.swap().unwrap().orders()[0]
}

/// Deliver an issued executor transaction as its broadcaster would.
async fn deliver(
    fork: &ForkChain,
    issued: &IssuedExecutorTransaction,
) -> alloy::rpc::types::TransactionReceipt {
    fork.send(
        issued
            .transaction()
            .clone()
            .from(VERIFICATION_BYPASS)
            .gas_limit(3_000_000),
    )
    .await
}

fn invalidates(call: &Call, settlement: Address, uid: OrderUid) -> bool {
    call.to == settlement
        && GPv2Settlement::invalidateOrderCall::abi_decode(&call.data)
            .is_ok_and(|call| call.orderUid[..] == uid.0[..])
}

/// `approve(spender, 0)` on `token`.
fn resets_approval(call: &Call, token: Address, spender: Address) -> bool {
    call.to == token
        && broadcaster_core::contracts::railgun::approveCall::abi_decode(&call.data)
            .is_ok_and(|call| call.spender == spender && call.amount.is_zero())
}

fn shields(call: &Call, executor: Address) -> bool {
    call.to == executor
        && broadcaster_core::contracts::railgun::shieldCall::abi_decode(&call.data).is_ok()
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_settlement_with_both_hooks_completes_at_finality() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let swap = wallet.swap(&fork).await;
    // Balanced share of the stub quote: the best case is 20,000,000 plus the 20,020 network
    // fee, less 0.5% tolerance (19,919,919), less 25% of a one-unit gas estimate at 1 wei.
    // The buy amount is the smallest one that nets that minimum after the shield fee.
    assert_eq!(swap.order.buyAmount, U256::from(19_919_918));

    let receipt = fork
        .settle(
            &swap.order,
            swap.signature.clone(),
            &swap.hooks.pre,
            &swap.hooks.post,
        )
        .await;
    assert!(receipt.status(), "the solver settles with both hooks");
    assert_eq!(fork.erc20_balance(USDC, swap.executor).await, U256::ZERO);
    let settled = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;

    let record = wallet.observe(swap.operation, settled..settled + 1).await;
    let observed = first_order(&record).observations();
    let settlement = Some(receipt.transaction_hash);
    assert_eq!(
        observed
            .pre_hook_executed
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(
        observed
            .traded
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(
        observed
            .delivered
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(swap_order_state(first_order(&record)), SwapOrderState::Done);
    assert!(record.reserved_inputs().contains(&swap.input));

    // A blocked reshield is refunded to the stealth account that funded it, not the solver.
    let shielded = receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.address() == RAILGUN)
        .filter_map(|log| log.log_decode::<Shield>().ok())
        .flat_map(|log| log.inner.data.commitments)
        .find(|preimage| preimage.token.tokenAddress == USDC)
        .map(|preimage| U256::from(preimage.value))
        .expect("the post-hook shields the bought USDC");
    let origin = crate::resolve_source_tx_origin(
        1,
        &wallet.chain,
        settled,
        receipt.transaction_hash,
        USDC,
        shielded,
        &HttpContext::direct_for_tests(),
    )
    .await
    .unwrap();
    assert_eq!(origin, swap.executor);
    assert_ne!(origin, receipt.from);
    wallet.finish().await;
}

/// `holder`'s balance of `token`, or of ETH for the native marker `Address::ZERO`.
async fn balance_of(fork: &ForkChain, token: Address, holder: Address) -> U256 {
    if token == Address::ZERO {
        fork.native_balance(holder).await
    } else {
        fork.erc20_balance(token, holder).await
    }
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_external_settlement_pays_the_receiver_and_completes_on_the_trade() {
    let fork = ForkChain::start().await;
    let mut wallet = Wallet::open(&fork);
    let receiver = Address::repeat_byte(0x77);
    // An ERC-20 buy, then a native buy that the settlement pays from its ETH balance.
    for buy in [USDC, Address::ZERO] {
        wallet.pair = (buy, SwapDelivery::External { receiver });
        let swap = wallet.swap(&fork).await;
        assert_eq!(swap.order.receiver, receiver);
        assert!(swap.hooks.post.is_empty());
        let before = balance_of(&fork, buy, receiver).await;

        let receipt = fork
            .settle(&swap.order, swap.signature.clone(), &swap.hooks.pre, &[])
            .await;
        assert!(
            receipt.status(),
            "the solver settles with the pre-hook alone"
        );
        assert!(balance_of(&fork, buy, receiver).await >= before + swap.order.buyAmount);
        assert_eq!(balance_of(&fork, buy, swap.executor).await, U256::ZERO);
        let settled = receipt.block_number.unwrap();
        fork.mine(wallet.chain.finality_depth).await;

        // The deployed settlement's Trade event alone completes the order.
        wallet
            .owner
            .observe_swap_settlement(swap.operation, swap.uid, settled)
            .await
            .unwrap();
        let record = wallet
            .owner
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == swap.operation)
            .unwrap();
        assert_eq!(swap_order_state(first_order(&record)), SwapOrderState::Done);
        assert_eq!(
            first_order(&record)
                .observations()
                .delivered
                .and_then(|observation| observation.transaction_hash),
            Some(receipt.transaction_hash)
        );
    }
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_early_pre_hook_then_fill_completes() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let swap = wallet.swap(&fork).await;

    // Someone runs the published pre-hook before a solver settles the order.
    let pre_hook = &swap.hooks.pre[0];
    let early = fork
        .send_as_bypass(
            swap.executor,
            pre_hook.call_data.clone(),
            pre_hook.gas_limit + 200_000,
        )
        .await;
    assert!(early.status(), "the pre-hook runs before its deadline");
    let ran = early.block_number.unwrap();
    // CoW's autopilot drops an order whose owner's balance or allowance after the pre-hook is
    // below `sellAmount`. The order sells exactly what the unshield left after Railgun's fee.
    let profile = wallet.chain.swap_profile().unwrap();
    assert_eq!(
        fork.erc20_balance(WETH, swap.executor).await,
        swap.order.sellAmount
    );
    assert_eq!(
        fork.call(
            WETH,
            ForkAllowance::allowanceCall {
                owner: swap.executor,
                spender: profile.vault_relayer(),
            },
        )
        .await,
        swap.order.sellAmount
    );
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet.observe(swap.operation, ran..ran + 1).await;
    assert_eq!(
        swap_order_state(first_order(&record)),
        SwapOrderState::PreHookOnly { expired: false }
    );

    // The funds and approval are already in place, so the solver settles without it.
    let receipt = fork
        .settle(&swap.order, swap.signature.clone(), &[], &swap.hooks.post)
        .await;
    assert!(receipt.status(), "the solver fills the order");
    assert_eq!(fork.erc20_balance(USDC, swap.executor).await, U256::ZERO);
    let settled = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;

    let record = wallet.observe(swap.operation, settled..settled + 1).await;
    let observed = first_order(&record).observations();
    assert_eq!(
        observed
            .pre_hook_executed
            .and_then(|observation| observation.transaction_hash),
        Some(early.transaction_hash)
    );
    let settlement = Some(receipt.transaction_hash);
    assert_eq!(
        observed
            .traded
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(
        observed
            .delivered
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(swap_order_state(first_order(&record)), SwapOrderState::Done);
    assert!(record.reserved_inputs().contains(&swap.input));
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_early_funded_post_hook_leaves_proceeds_for_recovery() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let swap = wallet.swap(&fork).await;

    // Someone funds the executor, and the solver runs the post-hook after the trade is
    // recorded but before the payout. The post-hook's guard passes on that funding and
    // shields it, then the settlement pays the proceeds to the executor. The trade and
    // the post-hook's shield land in one block, so only the balance shows the proceeds
    // are still public.
    fork.add_erc20(USDC, swap.executor, swap.order.buyAmount)
        .await;
    let receipt = fork
        .settle_with_intra_hooks(
            &swap.order,
            swap.signature.clone(),
            &swap.hooks.pre,
            &swap.hooks.post,
            &[],
        )
        .await;
    assert!(
        receipt.status(),
        "the solver runs the post-hook before the payout"
    );
    let settled = receipt.block_number.unwrap();
    // A reverted post-hook would have left the funding next to the proceeds.
    let held = fork.erc20_balance(USDC, swap.executor).await;
    assert_eq!(held, swap.order.buyAmount);
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet.observe(swap.operation, settled..settled + 1).await;
    assert_eq!(
        first_order(&record)
            .observations()
            .traded
            .and_then(|observation| observation.transaction_hash),
        Some(receipt.transaction_hash)
    );
    assert_eq!(
        swap_order_state(first_order(&record)),
        SwapOrderState::NotDelivered
    );

    // Recovery is offered. Both hooks used their nonces and the traded order can't fill
    // again, so the batch only shields, at k + 2.
    let prepared = wallet
        .prepare_recovery(swap.operation, Some(USDC), held)
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record) + U256::from(2)
        }
    );
    assert_eq!(prepared.calls().len(), 1);
    assert!(shields(&prepared.calls()[0], swap.executor));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    assert!(
        deliver(&fork, &issued).await.status(),
        "the recovery shields the proceeds"
    );
    assert_eq!(fork.erc20_balance(USDC, swap.executor).await, U256::ZERO);
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_retry_invalidates_an_order_stalled_by_an_older_post_hook() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let first = wallet.swap(&fork).await;
    let (operation, executor) = (first.operation, first.executor);
    let profile = wallet.chain.swap_profile().unwrap();
    let store = ExecutorStore::new(wallet.db.clone(), wallet.view.clone(), 1).unwrap();
    let state_of = |record: &ExecutorRecord, index: usize| {
        swap_order_state(&record.swap().unwrap().orders()[index])
    };
    let filled = |uid: OrderUid| {
        fork.call(
            profile.settlement(),
            ForkSwapSettlement::filledAmountCall {
                orderUid: uid.0.to_vec().into(),
            },
        )
    };

    // A cancellation wins the first pre-hook's nonce k and invalidates its order.
    let current = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    let observed = current.nonce_observation().unwrap();
    let tree = fork.railgun_tree().await;
    let fee = wallet.note(tree, U256::ONE);
    let cancellation = railgun_wallet::TransactionCall {
        to: executor,
        data: RelayAdapt7702::executeCall {
            _transactions: vec![synthetic_transaction(
                tree,
                wallet.nullifier(&fee),
                executor,
                None,
            )],
            _actionData: RelayAdapt7702ActionData {
                requireSuccess: true,
                minGasLimit: U256::ZERO,
                calls: vec![Call {
                    to: profile.settlement(),
                    value: U256::ZERO,
                    data: GPv2Settlement::invalidateOrderCall {
                        orderUid: first.uid.0.to_vec().into(),
                    }
                    .abi_encode()
                    .into(),
                }],
            },
            _nonce: observed.nonce(),
            _signature: Bytes::new(),
        }
        .abi_encode()
        .into(),
    };
    let (hash, signed) = wallet
        .owner
        .sign_swap_execute_for_tests(operation, &password(), &cancellation)
        .unwrap();
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                observed.nonce(),
                current.delegate(),
                hash,
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(
                    signed.clone(),
                    observed,
                    vec![ExecutorInputIdentity::from_utxo(&fee)],
                ),
            ),
        )
        .unwrap();
    let receipt = fork.send_as_bypass(executor, signed, 3_000_000).await;
    assert!(receipt.status(), "the cancellation wins nonce k");
    let cancelled = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    let observed = wallet.observe(operation, cancelled..cancelled + 1).await;
    assert_eq!(
        state_of(&observed, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)
    );
    assert_eq!(filled(first.uid).await, U256::MAX);
    // The cancelled order can't fill, so the retry's pre-hook is unchanged.
    assert_eq!(
        crate::swap_invalidation(&observed, &profile, std::time::SystemTime::now()).unwrap(),
        None
    );

    // The retry signs at k + 1 with the same proof, where the first post-hook is also valid.
    let delegated = wallet.redelegate(operation, cancelled).await;
    let retry = wallet
        .submit(&fork, delegated, first.note.clone(), None, None)
        .await;

    // Someone funds the executor and runs that older post-hook through another contract, so
    // no direct call to the executor identifies it. The retry's pre-hook is dead, but its
    // order can still fill until `validTo`.
    fork.add_erc20(USDC, executor, first.order.buyAmount).await;
    let post_hook = &first.hooks.post[0];
    let receipt = fork
        .send_as_bypass(
            MULTICALL3,
            IMulticall3::aggregate3Call {
                calls: vec![IMulticall3::Call3 {
                    target: executor,
                    allowFailure: false,
                    callData: post_hook.call_data.clone(),
                }],
            }
            .abi_encode()
            .into(),
            post_hook.gas_limit + 300_000,
        )
        .await;
    assert!(receipt.status(), "the funded older post-hook takes k + 1");
    assert_eq!(fork.erc20_balance(USDC, executor).await, U256::ZERO);
    let stalled_at = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    let observed = wallet.observe(operation, stalled_at..stalled_at + 1).await;
    assert_eq!(
        state_of(&observed, 1),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::OlderPostHook)
    );
    assert!(!observed.reserved_inputs().contains(&first.input));
    assert_eq!(filled(retry.uid).await, U256::ZERO);
    let invalidates =
        crate::swap_invalidation(&observed, &profile, std::time::SystemTime::now()).unwrap();
    assert_eq!(invalidates, Some(retry.uid));

    // The next retry is admitted at k + 2 although the older post-hook never won a direct
    // call, and its pre-hook invalidates the stalled order.
    let delegated = wallet.redelegate(operation, stalled_at).await;
    let latest = wallet
        .submit(&fork, delegated, first.note, None, invalidates)
        .await;
    let calls = RelayAdapt7702::executeCall::abi_decode(&latest.hooks.pre[0].call_data)
        .unwrap()
        ._actionData
        .calls;
    assert!(calls.iter().any(|call| {
        call.to == profile.settlement()
            && GPv2Settlement::invalidateOrderCall::abi_decode(&call.data)
                .is_ok_and(|call| call.orderUid[..] == retry.uid.0[..])
    }));

    // The latest pre-hook funds the executor and invalidates the stalled order at once, so a
    // solver can't fill the stalled order with those funds.
    let pre_hook = &latest.hooks.pre[0];
    let receipt = fork
        .send_as_bypass(
            executor,
            pre_hook.call_data.clone(),
            pre_hook.gas_limit + 200_000,
        )
        .await;
    assert!(
        receipt.status(),
        "the latest pre-hook runs before its deadline"
    );
    let funded = receipt.block_number.unwrap();
    assert!(fork.erc20_balance(WETH, executor).await >= latest.order.sellAmount);
    assert_eq!(filled(retry.uid).await, U256::MAX);
    let receipt = fork
        .settle(
            &retry.order,
            retry.signature.clone(),
            &retry.hooks.pre,
            &retry.hooks.post,
        )
        .await;
    assert!(!receipt.status(), "the stalled order can't fill");
    assert!(fork.erc20_balance(WETH, executor).await >= latest.order.sellAmount);

    let receipt = fork
        .settle(
            &latest.order,
            latest.signature.clone(),
            &latest.hooks.pre,
            &latest.hooks.post,
        )
        .await;
    assert!(receipt.status(), "the latest order settles");
    let settled = receipt.block_number.unwrap();
    assert_eq!(filled(latest.uid).await, latest.order.sellAmount);
    assert_eq!(filled(retry.uid).await, U256::MAX);
    fork.mine(wallet.chain.finality_depth).await;
    let observed = wallet.observe(operation, funded..settled + 1).await;
    assert_eq!(state_of(&observed, 2), SwapOrderState::Done);
    assert_eq!(
        state_of(&observed, 1),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::OlderPostHook)
    );
    assert!(
        observed.swap().unwrap().orders()[1]
            .observations()
            .traded
            .is_none()
    );
    drop(store);
    wallet.finish().await;
}

fn pre_hook_nonce(record: &ExecutorRecord) -> U256 {
    first_order(record).pre_hook().nonce()
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_recovery_after_expiry_resets_the_approval_and_skips_dead_pre_hooks() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let profile = wallet.chain.swap_profile().unwrap();
    let early = wallet.swap(&fork).await;
    let unused = wallet.swap(&fork).await;

    let pre_hook = &early.hooks.pre[0];
    let receipt = fork
        .send_as_bypass(
            early.executor,
            pre_hook.call_data.clone(),
            pre_hook.gas_limit + 200_000,
        )
        .await;
    assert!(receipt.status(), "the pre-hook runs before its deadline");
    let ran = receipt.block_number.unwrap();
    fork.increase_time(15 * 60).await;
    fork.mine(wallet.chain.finality_depth + 1).await;
    let confirmed = fork.block_number().await - wallet.chain.finality_depth;
    let record = wallet.observe(early.operation, ran..confirmed + 1).await;
    assert_eq!(
        swap_order_state(first_order(&record)),
        SwapOrderState::PreHookOnly { expired: true }
    );
    assert!(record.reserved_inputs().contains(&early.input));

    // The expired order can't fill, so the batch resets the approval and shields, at the
    // post-hook's nonce k + 1, without an invalidation.
    let held = fork.erc20_balance(WETH, early.executor).await;
    let prepared = wallet
        .prepare_recovery(early.operation, Some(WETH), held)
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record) + U256::ONE
        }
    );
    let calls = prepared.calls();
    assert_eq!(calls.len(), 2);
    assert!(resets_approval(&calls[0], WETH, profile.vault_relayer()));
    assert!(shields(&calls[1], early.executor));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    let receipt = deliver(&fork, &issued).await;
    assert!(receipt.status(), "the recovery shields the sell token");
    assert_eq!(fork.erc20_balance(WETH, early.executor).await, U256::ZERO);
    assert_eq!(
        fork.call(
            WETH,
            ForkAllowance::allowanceCall {
                owner: early.executor,
                spender: profile.vault_relayer(),
            },
        )
        .await,
        U256::ZERO
    );
    let recovered = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet
        .observe(early.operation, recovered..recovered + 1)
        .await;
    assert_eq!(
        record.payload_status(issued.payload_hash()),
        Some(ExecutorPayloadStatus::Executed)
    );

    // An expired pre-hook with its nonce k unused is not outstanding: recovery of tokens sent
    // to its executor warns of no competing payload and adds no step for that pre-hook.
    let record = wallet
        .observe(unused.operation, confirmed..confirmed + 1)
        .await;
    assert_eq!(
        swap_order_state(first_order(&record)),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    assert!(!record.reserved_inputs().contains(&unused.input));
    assert!(!record.has_competing_payloads());
    fork.add_erc20(WETH, unused.executor, U256::from(SELL_AMOUNT))
        .await;
    let prepared = wallet
        .prepare_recovery(unused.operation, Some(WETH), U256::from(SELL_AMOUNT))
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record)
        }
    );
    assert_eq!(prepared.calls().len(), 1);
    assert!(shields(&prepared.calls()[0], unused.executor));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    assert!(deliver(&fork, &issued).await.status());
    assert_eq!(fork.erc20_balance(WETH, unused.executor).await, U256::ZERO);
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_recovery_shields_the_buy_token_a_skipped_post_hook_left() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let swap = wallet.swap(&fork).await;

    let receipt = fork
        .settle(&swap.order, swap.signature.clone(), &swap.hooks.pre, &[])
        .await;
    assert!(receipt.status(), "the solver settles without the post-hook");
    let settled = receipt.block_number.unwrap();
    let held = fork.erc20_balance(USDC, swap.executor).await;
    assert_eq!(held, swap.order.buyAmount);
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet.observe(swap.operation, settled..settled + 1).await;
    assert_eq!(
        swap_order_state(first_order(&record)),
        SwapOrderState::NotDelivered
    );

    // The traded order can't fill again and the settlement used the whole approval, so the
    // batch only shields, competing with the post-hook at k + 1.
    let prepared = wallet
        .prepare_recovery(swap.operation, Some(USDC), held)
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record) + U256::ONE
        }
    );
    assert_eq!(prepared.calls().len(), 1);
    assert!(shields(&prepared.calls()[0], swap.executor));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    let receipt = deliver(&fork, &issued).await;
    assert!(receipt.status(), "the recovery shields the buy token");
    assert_eq!(fork.erc20_balance(USDC, swap.executor).await, U256::ZERO);
    let recovered = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet
        .observe(swap.operation, recovered..recovered + 1)
        .await;
    assert_eq!(
        record.payload_status(issued.payload_hash()),
        Some(ExecutorPayloadStatus::Executed)
    );
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_early_cancellation_wins_and_releases_the_inputs_at_finality() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let profile = wallet.chain.swap_profile().unwrap();
    let swap = wallet.swap(&fork).await;
    let record = wallet
        .owner
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == swap.operation)
        .unwrap();

    // A paid execute at the pre-hook's nonce k whose only action is the invalidation.
    let prepared = wallet
        .prepare_recovery(swap.operation, None, U256::ZERO)
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record)
        }
    );
    assert!(prepared.shield().is_none());
    assert_eq!(prepared.calls().len(), 1);
    assert!(invalidates(
        &prepared.calls()[0],
        profile.settlement(),
        swap.uid
    ));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    let receipt = deliver(&fork, &issued).await;
    assert!(receipt.status(), "the cancellation wins nonce k");
    let cancelled = receipt.block_number.unwrap();
    assert_eq!(
        fork.call(
            profile.settlement(),
            ForkSwapSettlement::filledAmountCall {
                orderUid: swap.uid.0.to_vec().into(),
            },
        )
        .await,
        U256::MAX
    );
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet
        .observe(swap.operation, cancelled..cancelled + 1)
        .await;
    assert_eq!(
        swap_order_state(first_order(&record)),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)
    );
    assert_eq!(
        record.payload_status(issued.payload_hash()),
        Some(ExecutorPayloadStatus::Executed)
    );
    assert!(!record.reserved_inputs().contains(&swap.input));
    let receipt = fork
        .settle(
            &swap.order,
            swap.signature.clone(),
            &swap.hooks.pre,
            &swap.hooks.post,
        )
        .await;
    assert!(!receipt.status(), "the cancelled order can't fill");
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_pre_hook_beats_the_cancellation_and_recovery_invalidates_the_order() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let profile = wallet.chain.swap_profile().unwrap();
    let swap = wallet.swap(&fork).await;
    let prepared = wallet
        .prepare_recovery(swap.operation, None, U256::ZERO)
        .await;
    let cancellation = wallet.issue_recovery(&fork, prepared).await;

    // Someone runs the published pre-hook before the cancellation lands.
    let pre_hook = &swap.hooks.pre[0];
    let receipt = fork
        .send_as_bypass(
            swap.executor,
            pre_hook.call_data.clone(),
            pre_hook.gas_limit + 200_000,
        )
        .await;
    assert!(receipt.status(), "the pre-hook takes nonce k");
    let ran = receipt.block_number.unwrap();
    let receipt = deliver(&fork, &cancellation).await;
    assert!(!receipt.status(), "the cancellation loses nonce k");
    let lost = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet.observe(swap.operation, ran..lost + 1).await;
    assert_eq!(
        swap_order_state(first_order(&record)),
        SwapOrderState::PreHookOnly { expired: false }
    );
    assert!(matches!(
        record.payload_status(cancellation.payload_hash()),
        Some(ExecutorPayloadStatus::Invalidated { .. })
    ));
    assert!(record.reserved_inputs().contains(&swap.input));

    // Recovery is offered. It invalidates the order that can still fill, resets the approval,
    // and shields the sell token, all in one execute at k + 1.
    let held = fork.erc20_balance(WETH, swap.executor).await;
    let prepared = wallet
        .prepare_recovery(swap.operation, Some(WETH), held)
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record) + U256::ONE
        }
    );
    let calls = prepared.calls();
    assert_eq!(calls.len(), 3);
    assert!(invalidates(&calls[0], profile.settlement(), swap.uid));
    assert!(resets_approval(&calls[1], WETH, profile.vault_relayer()));
    assert!(shields(&calls[2], swap.executor));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    assert!(deliver(&fork, &issued).await.status(), "the recovery runs");
    assert_eq!(fork.erc20_balance(WETH, swap.executor).await, U256::ZERO);
    assert_eq!(
        fork.call(
            profile.settlement(),
            ForkSwapSettlement::filledAmountCall {
                orderUid: swap.uid.0.to_vec().into(),
            },
        )
        .await,
        U256::MAX
    );
    assert_eq!(
        fork.call(
            WETH,
            ForkAllowance::allowanceCall {
                owner: swap.executor,
                spender: profile.vault_relayer(),
            },
        )
        .await,
        U256::ZERO
    );
    let receipt = fork
        .settle(
            &swap.order,
            swap.signature.clone(),
            &swap.hooks.pre,
            &swap.hooks.post,
        )
        .await;
    assert!(!receipt.status(), "the order can't fill after recovery");
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_across_settlement_deposits_the_approved_terms() {
    let fork = ForkChain::start().await;
    let mut wallet = Wallet::open(&fork);
    let spoke_pool = wallet.chain.bridge_profile().unwrap().spoke_pool();
    // Each settlement pays a surplus above the order's limit. The post-hook deposits exactly
    // the limit, then shields the surplus to the wallet or leaves it in the account.
    for surplus in [BridgeSurplus::Reshield, BridgeSurplus::KeepInAccount] {
        let reshielded = surplus == BridgeSurplus::Reshield;
        let (swap, receipt, terms) = wallet.settle_across(&fork, surplus).await;
        assert_eq!(swap.order.receiver, swap.executor);
        assert_eq!(
            (terms.spoke_pool, terms.input_token, terms.output_token),
            (spoke_pool, USDC, ARBITRUM_USDC)
        );
        assert_eq!(
            (terms.input_amount, terms.output_amount),
            (swap.order.buyAmount, U256::from(DESTINATION_MINIMUM))
        );

        let deposits = receipt
            .inner
            .logs()
            .iter()
            .filter(|log| log.address() == spoke_pool)
            .filter_map(|log| log.log_decode::<SpokePool::FundsDeposited>().ok())
            .map(|log| log.inner.data)
            .collect::<Vec<_>>();
        let [deposit] = deposits.as_slice() else {
            panic!("the post-hook deposits once");
        };
        assert_eq!(
            (deposit.depositor, deposit.recipient),
            (
                address_to_bytes32(swap.executor),
                address_to_bytes32(BRIDGE_RECEIVER)
            )
        );
        assert_eq!(
            (
                deposit.inputToken,
                deposit.outputToken,
                deposit.destinationChainId
            ),
            (
                address_to_bytes32(terms.input_token),
                address_to_bytes32(terms.output_token),
                U256::from(ARBITRUM_ONE)
            )
        );
        assert_eq!(
            (deposit.inputAmount, deposit.outputAmount),
            (terms.input_amount, terms.output_amount)
        );
        assert_eq!(
            (deposit.quoteTimestamp, deposit.fillDeadline),
            (terms.quote_timestamp, terms.fill_deadline)
        );
        assert_eq!(
            fork.erc20_balance(USDC, swap.executor).await,
            if reshielded {
                U256::ZERO
            } else {
                U256::from(SURPLUS)
            }
        );

        let record = wallet.record(swap.operation);
        let observed = first_order(&record).observations();
        assert_eq!(
            observed.trade_amounts.map(|amounts| amounts.buy_amount),
            Some(swap.order.buyAmount + U256::from(SURPLUS))
        );
        assert_eq!(
            observed
                .bridge_handoff
                .map(|handoff| (handoff.observation.transaction_hash, handoff.deposit_id)),
            Some((Some(receipt.transaction_hash), Some(deposit.depositId)))
        );
        // Only reshielded surplus is a private credit, net of Railgun's shield fee.
        assert_eq!(
            observed
                .settlement_credit
                .map(|credit| credit.private_amount + credit.fee.unwrap()),
            reshielded.then_some(U256::from(SURPLUS))
        );
        assert_eq!(
            swap_order_state(first_order(&record)),
            SwapOrderState::Bridging
        );
    }
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_across_order_app_data_stays_within_the_planned_length() {
    let fork = ForkChain::start().await;
    let mut wallet = Wallet::open(&fork);
    // Reshielded surplus gives an Across order its largest post-hook.
    wallet.pair = (USDC, across_delivery(BridgeSurplus::Reshield));
    let budget = wallet.chain.swap_profile().unwrap().app_data_byte_budget();
    // Notes in separate trees, each spent by its own pre-hook transaction. These orders are
    // signed but never settled, so the trees' roots don't matter.
    let notes = |count: u16| {
        (0..count)
            .map(|number| {
                wallet.note(
                    RailgunTree {
                        number,
                        root: B256::ZERO,
                    },
                    U256::from(2 * SELL_AMOUNT),
                )
            })
            .collect::<Vec<_>>()
    };

    for (count, amount) in [(1, SELL_AMOUNT), (2, 3 * SELL_AMOUNT)] {
        let delegated = wallet.delegate(&fork).await;
        let notes = notes(count);
        let SwapAmountPlan::Fits(plan) = wallet.plan(delegated, &notes, U256::from(amount), None)
        else {
            panic!("{count} transactions fit one order");
        };
        assert_eq!(plan.transaction_count(), usize::from(count));
        let planned = plan.app_data_len();
        let (inputs, transactions) = wallet.synthetic_pre_hook(&plan, &notes);
        let signed = wallet
            .sign(&fork, plan, &inputs, Some(transactions))
            .await
            .app_data_len;
        assert!(
            signed <= planned && planned <= budget,
            "{count} transactions: signed {signed}, planned {planned}, budget {budget}"
        );
        eprintln!("Across app data, {count} transactions: signed {signed}, planned {planned}");
    }

    // An amount that needs nine transactions, one more than a batch allows. The largest amount
    // offered instead fits the budget, and so does its signed order.
    let delegated = wallet.delegate(&fork).await;
    let notes = notes(9);
    let SwapAmountPlan::TooLarge { largest } =
        wallet.plan(delegated, &notes, U256::from(18 * SELL_AMOUNT), None)
    else {
        panic!("nine transactions don't fit one order");
    };
    let (count, planned) = (largest.transaction_count(), largest.app_data_len());
    let (inputs, transactions) = wallet.synthetic_pre_hook(&largest, &notes);
    let signed = wallet
        .sign(&fork, largest, &inputs, Some(transactions))
        .await
        .app_data_len;
    assert!(
        signed <= planned && planned <= budget,
        "largest offer, {count} transactions: signed {signed}, planned {planned}, budget {budget}"
    );
    eprintln!(
        "Across app data, largest offer of {count} transactions: signed {signed}, planned {planned}"
    );
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_refunded_across_deposit_is_recovered_to_the_wallet() {
    let fork = ForkChain::start().await;
    let mut wallet = Wallet::open(&fork);
    let (swap, receipt, terms) = wallet.settle_across(&fork, BridgeSurplus::Reshield).await;
    let settled = receipt.block_number.unwrap();
    assert_eq!(fork.erc20_balance(USDC, swap.executor).await, U256::ZERO);

    // The deposit expires unfilled, and the SpokePool refunds its input to the depositor.
    fork.impersonate(terms.spoke_pool).await;
    let refund = fork
        .send(
            alloy::rpc::types::TransactionRequest::default()
                .from(terms.spoke_pool)
                .to(USDC)
                .input(
                    ForkTransfer::transferCall {
                        to: swap.executor,
                        amount: terms.input_amount,
                    }
                    .abi_encode()
                    .into(),
                )
                .gas_limit(200_000),
        )
        .await;
    assert!(refund.status(), "the SpokePool refunds the deposit");
    let refunded = refund.block_number.unwrap();
    let store = ExecutorStore::new(wallet.db.clone(), wallet.view.clone(), 1).unwrap();
    let record = store
        .record_swap_bridge_outcome(swap.operation, swap.uid, SwapBridgeOutcome::Refunding)
        .unwrap();
    assert_eq!(
        swap_order_state(first_order(&record)),
        SwapOrderState::Refunding
    );

    // The explicit check reconciles the account at a final block that holds the refund.
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet.observe(swap.operation, settled..refunded + 1).await;
    assert_eq!(
        swap_order_state(first_order(&record)),
        SwapOrderState::Refunding
    );
    let held = fork.erc20_balance(USDC, swap.executor).await;
    assert_eq!(held, terms.input_amount);

    // Both hooks used their nonces and the traded order can't fill again, so the batch only
    // shields the refund, at k + 2.
    let prepared = wallet
        .prepare_recovery(swap.operation, Some(USDC), held)
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record) + U256::from(2)
        }
    );
    assert_eq!(prepared.calls().len(), 1);
    assert!(shields(&prepared.calls()[0], swap.executor));
    let npk = prepared.shield().unwrap().preimage.npk;
    let issued = wallet.issue_recovery(&fork, prepared).await;
    let receipt = deliver(&fork, &issued).await;
    assert!(receipt.status(), "the recovery shields the refund");
    assert_eq!(fork.erc20_balance(USDC, swap.executor).await, U256::ZERO);
    // The wallet's new note holds the refund less Railgun's shield fee.
    let credits = receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.address() == RAILGUN)
        .filter_map(|log| log.log_decode::<Shield>().ok())
        .flat_map(|log| {
            let event = log.inner.data;
            event.commitments.into_iter().zip(event.fees)
        })
        .filter(|(preimage, _)| preimage.npk == npk)
        .map(|(preimage, fee)| (U256::from(preimage.value), fee))
        .collect::<Vec<_>>();
    let [(credited, fee)] = credits.as_slice() else {
        panic!("the recovery shields once to the wallet");
    };
    assert!(!fee.is_zero());
    assert_eq!(*credited + *fee, held);
    let recovered = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet
        .observe(swap.operation, recovered..recovered + 1)
        .await;
    assert_eq!(
        record.payload_status(issued.payload_hash()),
        Some(ExecutorPayloadStatus::Executed)
    );
    drop(store);
    wallet.finish().await;
}
