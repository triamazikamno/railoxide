//! Opt-in mainnet-fork scenarios for swap outcomes, driven through the wallet's own
//! setup, signing, and observation paths with synthetic Railgun proofs:
//!
//! `ETH_FORK_RPC_URL=<mainnet RPC> cargo test -p wallet-ops swap_fork -- --ignored`
//!
//! Set `ANVIL_BIN` when `anvil` is not on `PATH`.
//!
//! The `swap_fork_private_across` scenarios also fork Polygon, the destination chain of their
//! private Bridge delivery, and replay the Across fill there. They read its RPC from
//! `DESTINATION_FORK_RPC_URL`.
//!
//! The `swap_fork_public` scenarios pay from a Public account on Ethereum. Those that deliver
//! to Polygon fork it from `DESTINATION_FORK_RPC_URL` too, and the one that delivers to BNB
//! Chain forks that chain from `BNB_FORK_RPC_URL`. Mainnet has no `SwapMath` yet, so a scenario
//! with an order deploys it on the Ethereum fork first, as its release will.
//!
//! The `swap_fork_public_permit` scenarios sell USDC through an order from a Public account
//! that holds no native balance, on Ethereum, Base and Arbitrum One. Each forks the chain it
//! pays on from `ETH_FORK_RPC_URL`, `BASE_FORK_RPC_URL` or `ARBITRUM_FORK_RPC_URL`, and Polygon,
//! where it delivers, from `DESTINATION_FORK_RPC_URL`:
//!
//! `BASE_FORK_RPC_URL=<Base RPC> DESTINATION_FORK_RPC_URL=<Polygon RPC> cargo test -p wallet-ops swap_fork_public_permit_order_on_base -- --ignored --nocapture`
//!
//! Each prints the gas its permit pre-hook used in the settlement to stderr.
//!
//! Settlements are sent by an impersonated allow-listed solver. The harness adds
//! `0xdEaD` to the `GPv2` solver allow list through the authenticator's manager,
//! because Railgun accepts a synthetic proof only when `tx.origin` is that
//! `VERIFICATION_BYPASS` address, and a settlement can run the swap's pre-hook.

use super::swap_order::{
    OutputPois, read_json_body, spawn_bridge_stub, spawn_orderbook, submitted_order,
};
use super::swap_setup::{
    DESTINATION_CHAIN, DESTINATION_TOKEN, USDC, WETH, broadcaster, password, setup_approval,
};
use super::*;
use crate::cow::{CowOrderbookClient, CowQuote};
use crate::public_wallet::VaultedPublicSigner;
use crate::signer::SoftwareEvmSigner;
use crate::tests::cow_fork::{
    ARBITRUM_FORK_RPC_URL_ENV, BASE_FORK_RPC_URL_ENV, BNB_FORK_RPC_URL_ENV,
    DESTINATION_FORK_RPC_URL_ENV, FORK_RPC_URL_ENV, ForkChain, HOOKS_TRAMPOLINE, MULTICALL3,
    RAILGUN, RailgunTree, SETTLEMENT, VERIFICATION_BYPASS, synthetic_transaction, unshield_to,
};
use crate::vault::{
    PublicAccountScope, PublicSwapApproval, PublicSwapDeposited, PublicSwapIntent,
    PublicSwapObservations, PublicSwapProxyHolding, PublicSwapRecord, PublicSwapTransactionKind,
    SwapAccountChoice, SwapAccountRole, SwapAccountUse, SwapApprovedAccount, SwapApprovedAccounts,
    SwapBridgeHandoff, SwapObservation, SwapUseId, SwapUseRecord, SwapUseRole,
};
use crate::{
    DelegatedSwapExecutor, ExecutorRecoveryExecution, ExecutorRecoveryFunding,
    IssuedExecutorTransaction, OperationHttpClient, OperationNetworkIsolation,
    PreparedExecutorOperation, PreparedExecutorRecovery, PublicActionGasFeeSelection,
    PublicSwapApprovalsOutcome, PublicSwapDelivery, PublicSwapDeliverySigning, PublicSwapGasPlan,
    PublicSwapOrderOutcome, PublicSwapOrderState, PublicSwapTransactionOutcome, PublicSwapUseClaim,
    SwapAmountPlan, SwapAmountRequest, SwapInputPlan, SwapOrderOutcome, SwapOrderState,
    SwapPairPreparation, SwapPairSide, SwapPrice, SwapSetupStatus, WalletNetworkMode,
    new_public_swap_batch_nonce, prepare_swap_pair, public_swap_order_state, swap_order_state,
};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use broadcaster_core::contracts::across::{
    MulticallHandler, SpokePool, address_to_bytes32, private_delivery_message,
};
use broadcaster_core::contracts::cow::{
    AppData, AppDataHook, AppDataHooks, BUY_NATIVE_TOKEN, GPv2Settlement, Order, OrderUid,
    order_uid,
};
use broadcaster_core::contracts::cow_shed::proxy_address;
use broadcaster_core::contracts::executor::{AcrossPrivateDelivery, EXECUTION_NONCE_STORAGE_SLOT};
use broadcaster_core::contracts::railgun::{Call, Shield, approveCall};
use broadcaster_core::contracts::swap_math::{
    DETERMINISTIC_DEPLOYER, SWAP_MATH_ADDRESS, SWAP_MATH_RUNTIME_CODE_HASH,
    swap_math_deployment_calldata,
};

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

    // The deployed `SpokePool`s fill with every address as a `bytes32`.
    interface ForkSpokePool {
        struct V3RelayData {
            bytes32 depositor;
            bytes32 recipient;
            bytes32 exclusiveRelayer;
            bytes32 inputToken;
            bytes32 outputToken;
            uint256 inputAmount;
            uint256 outputAmount;
            uint256 originChainId;
            uint256 depositId;
            uint32 fillDeadline;
            uint32 exclusivityDeadline;
            bytes message;
        }
        function fillRelay(V3RelayData relayData, uint256 repaymentChainId, bytes32 repaymentAddress) external;
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
/// The delivery allowance reviewed with every private Across order here, in USDC base units.
const DELIVERY_ALLOWANCE: u64 = 500_000;
/// The relayer that fills deposits on the destination fork.
const RELAYER: Address = Address::repeat_byte(0x5e);
/// Gas limit of a fill, above a handler fill that shields.
const FILL_GAS: u64 = 3_000_000;

/// Across delivery of bought USDC as Arbitrum One USDC, with `surplus` handled as chosen.
const fn across_delivery(surplus: BridgeSurplus) -> SwapDelivery {
    SwapDelivery::Bridge(BridgeDelivery {
        provider: BridgeProvider::Across,
        destination_chain: ARBITRUM_ONE,
        receiver: BRIDGE_RECEIVER,
        destination_token: ARBITRUM_USDC,
        surplus,
        private: None,
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
    /// The swap use the order was signed for.
    swap_use: SwapUseId,
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
    /// The wallet on the destination chain of private Across orders.
    destination: Option<Destination>,
    /// The delegated destination stealth account that new private Across orders deliver to.
    destination_account: Option<DelegatedSwapExecutor>,
    /// The use new orders are signed for, once a swap reuses the wallet's accounts. Until then
    /// an order belongs to its account's first use.
    swap_use: Option<SwapUseId>,
    /// The wallet's notes on the destination chain, which a reused destination account's
    /// earlier shields are judged from.
    destination_notes: Option<AcceptedShieldNotes>,
}

/// The destination chain of private Across orders over its own fork, and the wallet's owner
/// there.
struct Destination {
    chain: crate::settings::EffectiveChainConfig,
    owner: ExecutorOwner,
}

/// The wallet's local notes on the destination chain, as a test states them. The fork wallet
/// has no synced session to read them from.
struct AcceptedShieldNotes(Vec<crate::WalletUtxo>);

impl crate::SwapShieldNotes for AcceptedShieldNotes {
    fn shield_notes(
        &self,
        shield: &crate::vault::SwapEarlierShield,
    ) -> Option<Vec<crate::WalletUtxo>> {
        Some(crate::notes_of_shield(&self.0, shield))
    }
}

impl Wallet {
    fn open(fork: &ForkChain) -> Self {
        Self::open_on(fork, 1)
    }

    /// The wallet on the chain `chain_id`, over `fork`.
    fn open_on(fork: &ForkChain, chain_id: u64) -> Self {
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
        let mut chain = chains.get(chain_id).cloned().unwrap();
        chain.rpc_route =
            crate::RpcChainRoute::new(chain_id, vec![fork.url()]).with_multicall(MULTICALL3);
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
            destination: None,
            destination_account: None,
            swap_use: None,
            destination_notes: None,
        }
    }

    /// Also open the wallet's owner on the destination chain, over `fork`.
    fn with_destination(self, fork: &ForkChain) -> Self {
        self.with_destination_on(fork, DESTINATION_CHAIN)
    }

    /// Also open the wallet's owner on the destination chain `chain_id`, over `fork`.
    fn with_destination_on(mut self, fork: &ForkChain, chain_id: u64) -> Self {
        let mut chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(chain_id)
        .cloned()
        .unwrap();
        chain.rpc_route =
            crate::RpcChainRoute::new(chain_id, vec![fork.url()]).with_multicall(MULTICALL3);
        let owner = ExecutorOwner::new(
            0,
            self.db.clone(),
            self.view.clone(),
            chain.clone(),
            HttpContext::direct_for_tests(),
        )
        .unwrap();
        self.destination = Some(Destination { chain, owner });
        self
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
        self.swap_from(fork, delegated).await
    }

    /// Sign, persist, and submit `delegated`'s order to a local orderbook stub.
    async fn swap_from(&self, fork: &ForkChain, delegated: DelegatedSwapExecutor) -> Swap {
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
                None,
                &authorization,
            )
            .await
            .unwrap();
        self.install(fork, &self.owner, &self.chain, &prepared, &authorization)
            .await
    }

    /// Reserve both stealth accounts of a private Across order to the destination chain, with
    /// `surplus` and a failed shield handled as chosen, and delegate each on its own fork with a
    /// real delegation-only setup. New swaps then deliver to the destination account. Returns
    /// the swap's own delegated executor.
    async fn delegate_private(
        &mut self,
        fork: &ForkChain,
        destination_fork: &ForkChain,
        surplus: BridgeSurplus,
        on_shield_failure: BridgeShieldFailure,
    ) -> DelegatedSwapExecutor {
        let destination = self.destination.as_ref().unwrap();
        let authorization = password();
        let destination_authorization = authorization.for_destination().unwrap();
        let delegate = self.chain.accepted_executor_profile().unwrap().delegate();
        let destination_delegate = destination
            .chain
            .accepted_executor_profile()
            .unwrap()
            .delegate();
        let mut destination_candidate = broadcaster(destination_delegate);
        destination_candidate.chain_id = DESTINATION_CHAIN;
        let mut approval = setup_approval(
            WETH,
            USDC,
            SwapDelivery::Bridge(BridgeDelivery {
                provider: BridgeProvider::Across,
                destination_chain: DESTINATION_CHAIN,
                receiver: Address::ZERO,
                destination_token: DESTINATION_TOKEN,
                surplus,
                private: Some(BridgePrivateDelivery { on_shield_failure }),
            }),
        );
        approval.bounds.destination_setup_fee = Some(U256::from(1_000));
        approval.bounds.source_setup_fee = Some(U256::from(1_000));
        approval.bounds.destination_shield_fee_bps = Some(crate::RAILGUN_PROTOCOL_FEE_BPS);
        let operation = ExecutorOperationId::random().unwrap();
        let prepared = prepare_swap_pair(
            &self.owner,
            Some(&destination.owner),
            SwapPairPreparation {
                use_id: SwapUseId::first(operation),
                source: SwapAccountChoice::New(operation),
                destination: Some(SwapAccountChoice::New(
                    ExecutorOperationId::random().unwrap(),
                )),
                candidate: Some(broadcaster(delegate)),
                destination_candidate: Some(destination_candidate),
                approval,
                authorization: &authorization,
                destination_authorization: Some(&destination_authorization),
            },
        )
        .await
        .unwrap();
        let (SwapPairSide::Setup(origin_setup), Some(SwapPairSide::Setup(destination_setup))) =
            (&prepared.origin, &prepared.destination)
        else {
            panic!("both fresh accounts need setup");
        };
        let delegated = self
            .install(fork, &self.owner, &self.chain, origin_setup, &authorization)
            .await;
        let destination_account = self
            .install(
                destination_fork,
                &destination.owner,
                &destination.chain,
                destination_setup,
                &destination_authorization,
            )
            .await;
        self.pair = (USDC, prepared.approval.delivery);
        self.destination_account = Some(destination_account);
        delegated
    }

    /// Claim both stealth accounts of the delivered private Across swap `placed` for another
    /// swap under a new use, without a setup on either chain, and admit each at its current
    /// nonce as the order path does. `notes` are the wallet's local notes on the destination
    /// chain. New swaps then belong to that use and deliver to the same destination account.
    /// Returns the swap's own account.
    async fn reuse_private(
        &mut self,
        fork: &ForkChain,
        destination_fork: &ForkChain,
        placed: &PrivateDeposit,
        notes: AcceptedShieldNotes,
    ) -> DelegatedSwapExecutor {
        let destination = self.destination.as_ref().unwrap();
        let authorization = password();
        let destination_authorization = authorization.for_destination().unwrap();
        let (operation, destination_operation) =
            (placed.swap.operation, placed.account.operation());
        let (executor, destination_executor) = (placed.swap.executor, placed.account.executor());

        // A claim judges recorded outcomes only. The wallet's routine observation records the
        // nonce that the first swap's fill consumed, and one account read at the confirmed head
        // stands in for it here.
        let SwapSetupStatus::Delegated(observed) = destination
            .owner
            .observe_swap_setup(destination_operation)
            .await
            .unwrap()
        else {
            panic!("the destination account stays delegated");
        };
        assert_eq!(
            observed.observed().nonce(),
            placed.account.observed().nonce() + U256::ONE
        );

        let SwapDelivery::Bridge(delivery) = self.pair.1 else {
            panic!("the reused accounts served a Bridge swap");
        };
        // Only a side that needs setup has a setup fee limit, and neither does. The receiver
        // is a placeholder until the preparation binds the destination account.
        let mut approval = setup_approval(
            WETH,
            USDC,
            SwapDelivery::Bridge(BridgeDelivery {
                receiver: Address::ZERO,
                ..delivery
            }),
        );
        approval.bounds.destination_shield_fee_bps = Some(crate::RAILGUN_PROTOCOL_FEE_BPS);
        let swap_use = SwapUseId::random().unwrap();
        let issued = || {
            (
                self.record(operation).issued().len(),
                self.destination_record(destination_operation)
                    .issued()
                    .len(),
            )
        };
        let issued_before = issued();
        let prepared = prepare_swap_pair(
            &self.owner,
            Some(&destination.owner),
            SwapPairPreparation {
                use_id: swap_use,
                source: SwapAccountChoice::Existing(operation),
                destination: Some(SwapAccountChoice::Existing(destination_operation)),
                approval,
                candidate: None,
                destination_candidate: None,
                authorization: &authorization,
                destination_authorization: Some(&destination_authorization),
            },
        )
        .await
        .unwrap();

        // Neither account takes a setup, and the preparation signs nothing. The approval binds
        // both existing accounts and delivers to the same destination account.
        let prepared_destination = prepared.destination.as_ref().unwrap();
        assert!(
            !prepared.origin.requires_setup() && !prepared_destination.requires_setup(),
            "an existing account takes no setup"
        );
        assert_eq!(
            (prepared.origin.executor(), prepared_destination.executor()),
            (executor, destination_executor)
        );
        let bound = |address| SwapApprovedAccount {
            address: Some(address),
            setup: false,
        };
        assert_eq!(
            prepared.approval.accounts,
            Some(SwapApprovedAccounts {
                source: bound(executor),
                destination: Some(bound(destination_executor)),
            })
        );
        assert_eq!(prepared.approval.delivery, self.pair.1);
        assert_eq!(issued(), issued_before);
        // Each account keeps its first use as history and is claimed by the new one, which did
        // not reserve it fresh.
        for record in [
            self.record(operation),
            self.destination_record(destination_operation),
        ] {
            assert_eq!(
                record
                    .swap_uses()
                    .iter()
                    .map(SwapUseRecord::id)
                    .collect::<Vec<_>>(),
                [SwapUseId::first(operation), swap_use]
            );
            assert_eq!(record.active_swap_use(), Some(swap_use));
            assert!(!record.swap_use(swap_use).unwrap().is_fresh());
        }

        // Each account is admitted for the use at its current nonce: the source from its
        // settled and delivered order, the destination also from the notes of its earlier
        // shield.
        let confirmed = fork.block_number().await - self.chain.finality_depth;
        let source = self
            .owner
            .reuse_swap_account(
                operation,
                confirmed,
                SwapAccountRole::Source,
                SwapAccountUse::Claimed(swap_use),
            )
            .await
            .unwrap()
            .delegated()
            .expect("the reused source account is confirmed as delegated");
        let confirmed = destination_fork.block_number().await - destination.chain.finality_depth;
        let destination_account = destination
            .owner
            .delegated_swap_destination(
                destination_operation,
                confirmed,
                self.chain.chain_id,
                operation,
                swap_use,
                destination_executor,
                Some(&notes),
            )
            .await
            .unwrap();
        self.pair = (USDC, prepared.approval.delivery);
        self.destination_account = Some(destination_account);
        self.swap_use = Some(swap_use);
        self.destination_notes = Some(notes);
        source
    }

    /// Deliver `prepared`'s delegation-only setup on `fork`, the fork of `owner`'s `chain`, and
    /// confirm it there.
    async fn install(
        &self,
        fork: &ForkChain,
        owner: &ExecutorOwner,
        chain: &crate::settings::EffectiveChainConfig,
        prepared: &PreparedExecutorOperation,
        authorization: &crate::DesktopPrivateSpendAuthorization,
    ) -> DelegatedSwapExecutor {
        let (operation, executor) = (prepared.operation(), prepared.context().executor);
        let tree = fork
            .railgun_tree_of(chain.require_railgun().unwrap().deployment.contract)
            .await;
        let fee = self.note(tree, U256::ONE);
        let mut payment = synthetic_transaction(tree, self.nullifier(&fee), executor, None);
        // Railgun rejects bound parameters of another chain.
        payment.boundParams.chainID = chain.chain_id;
        let setup = railgun_wallet::TransactionCall {
            to: executor,
            data: RelayAdapt7702::executeCall {
                _transactions: vec![payment],
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
        let issued = owner
            .issue_operation(prepared, &setup, std::slice::from_ref(&fee), authorization)
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
        fork.mine(chain.finality_depth).await;
        let SwapSetupStatus::Delegated(delegated) =
            owner.observe_swap_setup(operation).await.unwrap()
        else {
            panic!("the confirmed setup delegated the executor");
        };
        delegated
    }

    /// The executor at its current confirmed nonce, for a retry.
    async fn redelegate(&self, operation: ExecutorOperationId) -> DelegatedSwapExecutor {
        let SwapSetupStatus::Delegated(delegated) =
            self.owner.observe_swap_setup(operation).await.unwrap()
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
        self.try_plan(delegated, notes, amount, invalidates)
            .unwrap()
    }

    /// [`Self::plan`], or the planner's error for an order that can't be planned at all.
    fn try_plan(
        &self,
        delegated: DelegatedSwapExecutor,
        notes: &[Utxo],
        amount: U256,
        invalidates: Option<OrderUid>,
    ) -> eyre::Result<SwapAmountPlan> {
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
    /// stub. A Bridge order is quoted by a local Across stub, and a private one's destination
    /// stealth account signs its shield first. Without `transactions`, a retry reuses the
    /// recorded proof.
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
        let bridge = match self.pair.1 {
            SwapDelivery::Bridge(delivery) => Some(delivery),
            _ => None,
        };
        let private = bridge.is_some_and(|delivery| delivery.is_private());
        if bridge.is_some() {
            review.set_bridge_for_tests(&crate::SwapBridgeQuote {
                provider: BridgeProvider::Across,
                destination_minimum: U256::from(DESTINATION_MINIMUM),
                expected_output: U256::from(DESTINATION_MINIMUM),
                fee: Some(U256::ZERO),
                leg: crate::BridgeLegPrice::SameAsset,
                fill_time_sec: None,
                private: private.then_some(crate::SwapPrivateBridgeQuote {
                    quoted_output: U256::from(DESTINATION_MINIMUM + DELIVERY_ALLOWANCE),
                    delivery_allowance: U256::from(DELIVERY_ALLOWANCE),
                    destination_shield_fee_bps: crate::RAILGUN_PROTOCOL_FEE_BPS,
                    deposit_floor: None,
                }),
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
        let across = match bridge {
            Some(delivery) => Some(
                self.across_stub(fork, &orderbook, delivery.destination_chain)
                    .await,
            ),
            None => None,
        };
        let destination = crate::bridge::BridgeDestination {
            destination_token: bridge.map_or(ARBITRUM_USDC, |delivery| delivery.destination_token),
            intermediate: USDC,
            symbol: "USDC".to_owned(),
            same_asset: true,
            near: None,
        };
        let route = across
            .as_ref()
            .map(|(clients, destination_chain, _)| crate::SwapBridgeRoute {
                clients,
                destination: &destination,
                destination_chain,
            });
        let destination_authorization = authorization.for_destination().unwrap();
        let destination_signing = private.then(|| crate::SwapDestinationSigning {
            owner: &self.destination.as_ref().unwrap().owner,
            delegated: self.destination_account.unwrap(),
            authorization: &destination_authorization,
            notes: self
                .destination_notes
                .as_ref()
                .map(|notes| notes as &dyn crate::SwapShieldNotes),
        });
        let swap_use = self.swap_use.unwrap_or_else(|| SwapUseId::first(operation));
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
                swap_use,
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
                destination_minimum: bridge.map(|_| U256::from(DESTINATION_MINIMUM)),
                destination: destination_signing,
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
            swap_use,
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
    /// destination chain `destination_chain`: the wallet's own over its fork, or else the
    /// default one. The quote is dated at the fork's latest block, which the `SpokePool` checks
    /// at the deposit, and its fill deadline is three hours later: past the order's expiry plus
    /// the wallet's margin, and within the `SpokePool`'s buffer. The stub answers a private
    /// order's signing-time request, which carries the handler and its message, the same way.
    async fn across_stub(
        &self,
        fork: &ForkChain,
        orderbook: &CowOrderbookClient,
        destination_chain: u64,
    ) -> (
        crate::SwapBridgeClients,
        crate::settings::EffectiveChainConfig,
        tokio::task::JoinHandle<()>,
    ) {
        let quoted = fork.timestamp().await;
        let spoke_pool = self.chain.bridge_profile().unwrap().spoke_pool();
        let destination = match &self.destination {
            Some(destination) if destination.chain.chain_id == destination_chain => {
                destination.chain.clone()
            }
            _ => crate::settings::build_effective_chain_configs(
                &crate::settings::WalletSettings::default(),
            )
            .unwrap()
            .get(destination_chain)
            .cloned()
            .unwrap(),
        };
        let destination_spoke_pool = destination.bridge_profile().unwrap().spoke_pool();
        let (url, _, task) = spawn_bridge_stub(move |_| {
            across_quote_at(
                spoke_pool,
                destination_spoke_pool,
                U256::from(DESTINATION_MINIMUM),
                quoted,
            )
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
        (clients, destination, task)
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

    /// The record of the account `operation` on the destination chain.
    fn destination_record(&self, operation: ExecutorOperationId) -> ExecutorRecord {
        self.destination
            .as_ref()
            .unwrap()
            .owner
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
        self.settle_deposit(fork, swap).await
    }

    /// Settle `swap`'s Across order through `GPv2Settlement` with both hooks, paying `SURPLUS`
    /// above its limit, and observe the settlement once it is final.
    async fn settle_deposit(
        &self,
        fork: &ForkChain,
        swap: Swap,
    ) -> (Swap, TransactionReceipt, AcrossOrderTerms) {
        let record = self.record(swap.operation);
        let Some(BridgeOrderTerms::Across(terms)) = order_of(&record, swap.uid).bridge().cloned()
        else {
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

    /// Place a private Across order with `surplus` and a failed shield handled as chosen, from
    /// fresh stealth accounts on both forks, and settle it like [`Self::settle_across`].
    async fn settle_private_across(
        &mut self,
        fork: &ForkChain,
        destination_fork: &ForkChain,
        surplus: BridgeSurplus,
        on_shield_failure: BridgeShieldFailure,
    ) -> (Swap, TransactionReceipt, AcrossOrderTerms) {
        let delegated = self
            .delegate_private(fork, destination_fork, surplus, on_shield_failure)
            .await;
        let swap = self.swap_from(fork, delegated).await;
        self.settle_deposit(fork, swap).await
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
        if let Some(destination) = self.destination {
            destination.owner.shutdown().await;
        }
        self.owner.shutdown().await;
        drop(self.owner);
        drop(self.view);
        drop(self.vault);
        drop(self.db);
        std::fs::remove_dir_all(self.root).unwrap();
    }
}

/// Across's fee quote of `output` for a deposit into `spoke_pool`, dated `quoted` with a fill
/// deadline three hours later.
fn across_quote_at(
    spoke_pool: Address,
    destination_spoke_pool: Address,
    output: U256,
    quoted: u64,
) -> String {
    json!({
        "outputAmount": output.to_string(),
        "totalRelayFee": {"pct": "0", "total": "10000"},
        "relayerGasFee": {"pct": "0", "total": "0"},
        "lpFee": {"pct": "0", "total": "0"},
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
}

fn first_order(record: &ExecutorRecord) -> &SwapOrderRecord {
    &record.swap().unwrap().orders()[0]
}

/// `record`'s order `uid`.
fn order_of(record: &ExecutorRecord, uid: OrderUid) -> &SwapOrderRecord {
    record
        .swap()
        .unwrap()
        .orders()
        .iter()
        .find(|order| order.uid() == uid)
        .unwrap()
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
    // Private sync shows the cancellation's fee note spent in its recorded transaction, which
    // is what names it at nonce k.
    store
        .record_submission(operation, hash, receipt.transaction_hash)
        .unwrap();
    let mut spent = railgun_wallet::WalletUtxo::new(fee);
    spent.spent = Some(UtxoSource {
        tx_hash: receipt.transaction_hash,
        block_number: cancelled,
        block_timestamp: 0,
    });
    let sync = sync_service::WalletCurrentSnapshot::new(
        cancelled,
        0,
        0,
        vec![spent],
        sync_service::WalletPendingOverlay::default(),
    );
    let observed = wallet
        .owner
        .observe_swap_synced(operation, cancelled..cancelled + 1, Some(sync))
        .await
        .unwrap()
        .record()
        .clone();
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
    let delegated = wallet.redelegate(operation).await;
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
    assert!(observed.reserved_inputs().contains(&first.input));
    let resolved_at = observed.nonce_watermark().unwrap().block();
    let observed = ExecutorStore::new(wallet.db.clone(), wallet.view.clone(), 1)
        .unwrap()
        .record_synced(operation, None, resolved_at)
        .unwrap();
    assert!(!observed.reserved_inputs().contains(&first.input));
    assert_eq!(filled(retry.uid).await, U256::ZERO);
    let invalidates =
        crate::swap_invalidation(&observed, &profile, std::time::SystemTime::now()).unwrap();
    assert_eq!(invalidates, Some(retry.uid));

    // The next retry is admitted at k + 2 although the older post-hook never won a direct
    // call, and its pre-hook invalidates the stalled order.
    let delegated = wallet.redelegate(operation).await;
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
        record.payload_state(issued.payload_hash()),
        Some(ExecutorPayloadState::Resolved)
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
        record.payload_state(issued.payload_hash()),
        Some(ExecutorPayloadState::Resolved)
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
    // The account read shows nonce k consumed. The order reads as invalidated while the
    // pre-hook's notes are unspent, and that state names the cancellation: no block or
    // receipt of its transaction is read.
    let record = wallet
        .observe(swap.operation, cancelled..cancelled + 1)
        .await;
    assert_eq!(
        swap_order_state(first_order(&record)),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)
    );
    assert_eq!(
        record.payload_state(issued.payload_hash()),
        Some(ExecutorPayloadState::Resolved)
    );
    // Every payload at nonce k keeps its notes until private sync has scanned the block of
    // that read.
    assert!(record.reserved_inputs().contains(&swap.input));
    let resolved_at = record.nonce_watermark().unwrap().block();
    let record = ExecutorStore::new(wallet.db.clone(), wallet.view.clone(), 1)
        .unwrap()
        .record_synced(swap.operation, None, resolved_at)
        .unwrap();
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
    // The pre-hook's settlement evidence names what ran. The cancellation's nonce is
    // consumed, so its signature can no longer execute.
    assert_eq!(
        record.payload_state(cancellation.payload_hash()),
        Some(ExecutorPayloadState::Resolved)
    );
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

/// The deposits `spoke_pool` took in `receipt`'s transaction.
fn deposits(receipt: &TransactionReceipt, spoke_pool: Address) -> Vec<SpokePool::FundsDeposited> {
    receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.address() == spoke_pool)
        .filter_map(|log| log.log_decode::<SpokePool::FundsDeposited>().ok())
        .map(|log| log.inner.data)
        .collect()
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

        let deposits = deposits(&receipt, spoke_pool);
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

/// `count` notes in separate trees, each spent by its own pre-hook transaction. Their orders are
/// signed but never settled, so the trees' roots don't matter.
fn separate_tree_notes(wallet: &Wallet, count: u16) -> Vec<Utxo> {
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
        .collect()
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_across_order_app_data_stays_within_the_planned_length() {
    let fork = ForkChain::start().await;
    let mut wallet = Wallet::open(&fork);
    // Reshielded surplus gives an Across order its largest post-hook.
    wallet.pair = (USDC, across_delivery(BridgeSurplus::Reshield));
    let budget = wallet.chain.swap_profile().unwrap().app_data_byte_budget();

    for (count, amount) in [(1, SELL_AMOUNT), (2, 3 * SELL_AMOUNT)] {
        let delegated = wallet.delegate(&fork).await;
        let notes = separate_tree_notes(&wallet, count);
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
    let notes = separate_tree_notes(&wallet, 9);
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

/// The surplus choices of an Across order, with their labels in measurements.
const SURPLUS_CHOICES: [(BridgeSurplus, &str); 2] = [
    (BridgeSurplus::KeepInAccount, "keep"),
    (BridgeSurplus::Reshield, "reshield"),
];

// A private Across order's post-hook also carries the handler message, which the plan sizes from
// a placeholder of the destination account's shield.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_private_across_order_app_data_stays_within_the_planned_length() {
    let fork = ForkChain::start().await;
    let destination_fork = ForkChain::start_destination(DESTINATION_CHAIN).await;
    let mut wallet = Wallet::open(&fork).with_destination(&destination_fork);
    let budget = wallet.chain.swap_profile().unwrap().app_data_byte_budget();

    for (surplus, label) in SURPLUS_CHOICES {
        // One pre-hook transaction, then an amount that needs nine, one more than a batch
        // allows, for the largest amount offered instead.
        for (note_count, amount) in [(1, SELL_AMOUNT), (9, 18 * SELL_AMOUNT)] {
            let delegated = wallet
                .delegate_private(
                    &fork,
                    &destination_fork,
                    surplus,
                    BridgeShieldFailure::RefundOnOrigin,
                )
                .await;
            let notes = separate_tree_notes(&wallet, note_count);
            let plan = match wallet.try_plan(delegated, &notes, U256::from(amount), None) {
                Ok(SwapAmountPlan::Fits(plan)) if notes.len() == 1 => plan,
                Ok(SwapAmountPlan::TooLarge { largest }) if notes.len() == 9 => largest,
                unplanned => {
                    // Not even this order's smallest pre-hook fits beside the handler message.
                    // Only a reshielded surplus, the larger post-hook, may leave no room.
                    let reported = match &unplanned {
                        Ok(SwapAmountPlan::Fits(_)) => "fits".to_owned(),
                        Ok(SwapAmountPlan::TooLarge { .. }) => "too large".to_owned(),
                        Err(error) => error.to_string(),
                    };
                    assert!(
                        surplus == BridgeSurplus::Reshield,
                        "{} notes with surplus kept in the account: {reported}",
                        notes.len()
                    );
                    println!(
                        "MEASURE private_across_app_data surplus={label} notes={} fits=false planner={reported:?}",
                        notes.len()
                    );
                    continue;
                }
            };
            let (count, planned) = (plan.transaction_count(), plan.app_data_len());
            let (inputs, transactions) = wallet.synthetic_pre_hook(&plan, &notes);
            let signed = wallet
                .sign(&fork, plan, &inputs, Some(transactions))
                .await
                .app_data_len;
            assert!(
                signed <= planned && planned <= budget,
                "surplus {label}, {count} transactions: signed {signed}, planned {planned}, budget {budget}"
            );
            println!(
                "MEASURE private_across_app_data surplus={label} transactions={count} signed={signed} planned={planned} budget={budget}"
            );
        }
    }
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
        record.payload_state(issued.payload_hash()),
        Some(ExecutorPayloadState::Resolved)
    );
    drop(store);
    wallet.finish().await;
}

/// A settled private Across order's deposit on the swap's chain, and what its fill runs on the
/// destination chain.
struct PrivateDeposit {
    swap: Swap,
    /// The settlement on the swap's chain.
    receipt: TransactionReceipt,
    terms: AcrossOrderTerms,
    deposit: SpokePool::FundsDeposited,
    /// The destination stealth account.
    account: DelegatedSwapExecutor,
    /// The account's recorded shield payload, which the deposit's message carries.
    shield: Bytes,
}

/// Settle a private Across order from fresh stealth accounts on both forks, and check its
/// deposit as [`placed_private_deposit`] does.
async fn private_deposit(
    wallet: &mut Wallet,
    fork: &ForkChain,
    destination_fork: &ForkChain,
    surplus: BridgeSurplus,
    on_shield_failure: BridgeShieldFailure,
) -> PrivateDeposit {
    let settled = wallet
        .settle_private_across(fork, destination_fork, surplus, on_shield_failure)
        .await;
    placed_private_deposit(wallet, settled, on_shield_failure)
}

/// The deposit of a settled private Across order, from fresh or reused stealth accounts. Its
/// post-hook must deposit, within its declared gas limit, for the destination chain's handler
/// with the message that drains the fill to the destination account and runs the shield that
/// account signed for the order's swap use, and the wallet must record the hand-off.
fn placed_private_deposit(
    wallet: &Wallet,
    (swap, receipt, terms): (Swap, TransactionReceipt, AcrossOrderTerms),
    on_shield_failure: BridgeShieldFailure,
) -> PrivateDeposit {
    let destination = wallet.destination.as_ref().unwrap();
    let account = wallet.destination_account.unwrap();
    let handler = destination
        .chain
        .bridge_profile()
        .unwrap()
        .multicall_handler();
    let destination_record = wallet.destination_record(account.operation());
    let Some(SwapUseRole::Destination {
        shields: signed, ..
    }) = destination_record
        .swap_use(swap.swap_use)
        .map(SwapUseRecord::role)
    else {
        panic!("the destination account serves the order's swap use");
    };
    let shields = destination_record
        .issued()
        .iter()
        .filter(|payload| {
            payload.purpose() == ExecutorPayloadPurpose::SwapDestinationShield
                && signed.contains(&payload.hash())
        })
        .map(|payload| payload.context().calldata().clone())
        .collect::<Vec<_>>();
    let [shield] = shields.as_slice() else {
        panic!("the destination account signs one shield for the swap use");
    };
    let fallback = match on_shield_failure {
        BridgeShieldFailure::RefundOnOrigin => None,
        BridgeShieldFailure::KeepOnDestination => Some(account.executor()),
    };
    let message = private_delivery_message(
        handler,
        DESTINATION_TOKEN,
        account.executor(),
        shield.clone(),
        fallback,
    );

    // The hooks trampoline drops a post-hook that runs out of gas, which leaves no deposit.
    let deposits = deposits(&receipt, terms.spoke_pool);
    let [deposit] = deposits.as_slice() else {
        panic!("the private post-hook deposits once within its gas limit");
    };
    assert_eq!(
        (deposit.depositor, deposit.recipient),
        (
            address_to_bytes32(swap.executor),
            address_to_bytes32(handler)
        )
    );
    assert_eq!(deposit.message, message);
    assert_eq!(
        (
            deposit.outputToken,
            deposit.outputAmount,
            deposit.destinationChainId
        ),
        (
            address_to_bytes32(DESTINATION_TOKEN),
            U256::from(DESTINATION_MINIMUM),
            U256::from(DESTINATION_CHAIN)
        )
    );
    assert_eq!(
        (terms.recipient, terms.message_hash),
        (Some(handler), Some(alloy::primitives::keccak256(&message)))
    );
    let record = wallet.record(swap.operation);
    assert_eq!(
        order_of(&record, swap.uid)
            .observations()
            .bridge_handoff
            .map(|handoff| (handoff.observation.transaction_hash, handoff.deposit_id)),
        Some((Some(receipt.transaction_hash), Some(deposit.depositId)))
    );
    PrivateDeposit {
        deposit: deposit.clone(),
        shield: shield.clone(),
        swap,
        receipt,
        terms,
        account,
    }
}

/// `deposit`'s relay on the destination chain, as that chain's `SpokePool` takes it in a fill.
fn relay_of(deposit: &SpokePool::FundsDeposited) -> ForkSpokePool::V3RelayData {
    ForkSpokePool::V3RelayData {
        depositor: deposit.depositor,
        recipient: deposit.recipient,
        exclusiveRelayer: deposit.exclusiveRelayer,
        inputToken: deposit.inputToken,
        outputToken: deposit.outputToken,
        inputAmount: deposit.inputAmount,
        outputAmount: deposit.outputAmount,
        originChainId: U256::ONE,
        depositId: deposit.depositId,
        fillDeadline: deposit.fillDeadline,
        exclusivityDeadline: deposit.exclusivityDeadline,
        message: deposit.message.clone(),
    }
}

/// Give the relayer destination-chain USDC for several fills, which `spoke_pool` may pull.
async fn fund_relayer(fork: &ForkChain, spoke_pool: Address) {
    fund_relayer_with(
        fork,
        spoke_pool,
        DESTINATION_TOKEN,
        U256::from(10 * DESTINATION_MINIMUM),
    )
    .await;
}

/// Give the relayer `amount` of `token` on `fork`'s chain, which `spoke_pool` may pull.
async fn fund_relayer_with(fork: &ForkChain, spoke_pool: Address, token: Address, amount: U256) {
    fork.impersonate(RELAYER).await;
    fork.add_erc20(token, RELAYER, amount).await;
    let receipt = fork
        .send(
            TransactionRequest::default()
                .from(RELAYER)
                .to(token)
                .input(
                    approveCall {
                        spender: spoke_pool,
                        amount: U256::MAX,
                    }
                    .abi_encode()
                    .into(),
                )
                .gas_limit(200_000),
        )
        .await;
    assert!(receipt.status(), "the relayer approves the SpokePool");
}

/// Fill `relay` through `spoke_pool` as the relayer. Also returns why the fill reverts, from a
/// simulation on the state before it, or `None` if it doesn't.
async fn fill(
    fork: &ForkChain,
    spoke_pool: Address,
    relay: ForkSpokePool::V3RelayData,
) -> (TransactionReceipt, Option<String>) {
    fill_on(fork, spoke_pool, relay, DESTINATION_CHAIN).await
}

/// [`fill`] on the fork of the chain `chain_id`, where the relayer is also repaid.
async fn fill_on(
    fork: &ForkChain,
    spoke_pool: Address,
    relay: ForkSpokePool::V3RelayData,
    chain_id: u64,
) -> (TransactionReceipt, Option<String>) {
    let request = TransactionRequest::default()
        .from(RELAYER)
        .to(spoke_pool)
        .input(
            ForkSpokePool::fillRelayCall {
                relayData: relay,
                repaymentChainId: U256::from(chain_id),
                repaymentAddress: address_to_bytes32(RELAYER),
            }
            .abi_encode()
            .into(),
        )
        .gas_limit(FILL_GAS);
    let reverts = fork.revert_reason(request.clone()).await;
    (fork.send(request).await, reverts)
}

/// `executor`'s execution nonce on `fork`.
async fn execution_nonce(fork: &ForkChain, executor: Address) -> U256 {
    fork.storage_at(executor, EXECUTION_NONCE_STORAGE_SLOT)
        .await
}

/// The amount and fee of each shield of `token` into `railgun` in `receipt`'s transaction.
fn shields_of(receipt: &TransactionReceipt, railgun: Address, token: Address) -> Vec<(U256, U256)> {
    receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.address() == railgun)
        .filter_map(|log| log.log_decode::<Shield>().ok())
        .flat_map(|log| {
            let event = log.inner.data;
            event.commitments.into_iter().zip(event.fees)
        })
        .filter(|(preimage, _)| preimage.token.tokenAddress == token)
        .map(|(preimage, fee)| (U256::from(preimage.value), fee))
        .collect()
}

/// The wallet's own verification of `placed`'s delivery once `filled`'s block is final on the
/// destination fork: one poll of an Across stub whose deposit record names that block, then the
/// read of its receipts. Returns the outcome recorded with the swap and the one the destination
/// account's record takes from it.
async fn verify_fill(
    wallet: &Wallet,
    destination_fork: &ForkChain,
    placed: &PrivateDeposit,
    filled: &TransactionReceipt,
) -> (Option<SwapBridgeOutcome>, Option<SwapDestinationOutcome>) {
    let destination = wallet.destination.as_ref().unwrap();
    destination_fork
        .mine(destination.chain.finality_depth)
        .await;
    let located = json!({"deposit": {
        "status": "filled",
        "fillBlockNumber": filled.block_number.unwrap(),
        "fillTx": filled.transaction_hash,
        "outputAmount": DESTINATION_MINIMUM.to_string(),
        "recipient": placed.terms.recipient.unwrap(),
        "destinationChainId": DESTINATION_CHAIN.to_string()
    }})
    .to_string();
    let (url, _, stub) = spawn_bridge_stub(move |_| located.clone()).await;
    let http = || {
        OperationHttpClient::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        )
    };
    // Tracking asks only Across and the destination chain, never the orderbook.
    let orderbook =
        CowOrderbookClient::new(http(), "http://127.0.0.1:1/mainnet".parse().unwrap(), 1).unwrap();
    let mut clients = wallet.owner.swap_bridge_clients(&orderbook).unwrap();
    clients.across = crate::bridge::AcrossClient::new(http(), url).unwrap();
    let outcome = wallet
        .owner
        .observe_swap_bridge(
            placed.swap.operation,
            placed.swap.uid,
            &clients,
            &destination.chain,
        )
        .await
        .unwrap();
    stub.abort();
    destination.owner.reconcile_swap_destinations().unwrap();
    let settled = destination
        .owner
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == placed.account.operation())
        .unwrap()
        .swap_destination()
        .unwrap()
        .outcome;
    (outcome, settled)
}

/// What the wallet keeps of `placed`'s swap use: its order, with its terms, identifier,
/// observations and bridge outcome, and the use's record on the source and on the destination
/// account, which holds the destination outcome.
fn use_history(
    wallet: &Wallet,
    placed: &PrivateDeposit,
) -> (
    SwapOrderRecord,
    Option<SwapUseRecord>,
    Option<SwapUseRecord>,
) {
    let source = wallet.record(placed.swap.operation);
    let destination = wallet.destination_record(placed.account.operation());
    let swap_use = placed.swap.swap_use;
    (
        order_of(&source, placed.swap.uid).clone(),
        source.swap_use(swap_use).cloned(),
        destination.swap_use(swap_use).cloned(),
    )
}

/// The note that `filled`'s shield of `value` of the destination token created, as the wallet's
/// synced notes hold it once every active POI list accepted it.
fn accepted_shield_note(
    wallet: &Wallet,
    filled: &TransactionReceipt,
    value: U256,
) -> crate::WalletUtxo {
    let mut utxo = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            wallet.view.scan_keys().master_public_key,
            DESTINATION_TOKEN,
            value,
            [9; 16],
        ),
        0,
        0,
        UtxoSource {
            tx_hash: filled.transaction_hash,
            block_number: filled.block_number.unwrap(),
            block_timestamp: 0,
        },
        UtxoCommitmentKind::Shield,
    );
    for list in poi::poi::default_active_poi_list_keys() {
        utxo.poi.statuses.insert(list, crate::PoiStatus::Valid);
    }
    crate::WalletUtxo::new(utxo)
}

/// The purpose and nonce of each payload that `record`'s account signed, in signing order.
fn issued_nonces(record: &ExecutorRecord) -> Vec<(ExecutorPayloadPurpose, U256)> {
    record
        .issued()
        .iter()
        .map(|payload| (payload.purpose(), payload.nonce()))
        .collect()
}

// A relayer's fill of a private Across deposit pays the destination chain's handler, which
// passes the tokens to the destination stealth account and runs its pre-signed shield, all in
// the fill's transaction. Before the fill that shield reverts for anyone without using the
// account's nonce. Without a fallback recipient, a shield that fails reverts the whole fill.
//
// Once that swap is delivered and verified, both of its accounts serve a second private swap
// under a new use, without a setup on either chain. Each signs at its current nonce, the second
// fill shields to the wallet as the first did, the first swap's payloads can't run again, and
// what the wallet keeps of the first swap stays as it was.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_private_across_fill_shields_to_the_wallet_on_the_destination_chain() {
    let fork = ForkChain::start().await;
    let destination_fork = ForkChain::start_destination(DESTINATION_CHAIN).await;
    let mut wallet = Wallet::open(&fork).with_destination(&destination_fork);
    let placed = private_deposit(
        &mut wallet,
        &fork,
        &destination_fork,
        BridgeSurplus::KeepInAccount,
        BridgeShieldFailure::RefundOnOrigin,
    )
    .await;
    let destination = wallet.destination.as_ref().unwrap();
    let profile = destination.chain.bridge_profile().unwrap();
    let (spoke_pool, handler) = (profile.spoke_pool(), profile.multicall_handler());
    let railgun = destination
        .chain
        .require_railgun()
        .unwrap()
        .deployment
        .contract;
    let account = placed.account.executor();
    let output = U256::from(DESTINATION_MINIMUM);
    let balance = |holder| destination_fork.erc20_balance(DESTINATION_TOKEN, holder);
    let nonce = execution_nonce(&destination_fork, account).await;
    assert_eq!(nonce, placed.account.observed().nonce());
    assert!(
        destination_fork.timestamp().await < u64::from(placed.terms.fill_deadline),
        "the destination fork's clock is before the fill deadline"
    );
    fund_relayer(&destination_fork, spoke_pool).await;
    let funded = balance(RELAYER).await;
    let held_by_handler = balance(handler).await;

    // The message is public from the deposit onward. Someone runs the shield before the fill.
    let snapshot = destination_fork.snapshot().await;
    let early = destination_fork
        .send_as_bypass(account, placed.shield.clone(), FILL_GAS)
        .await;
    assert!(
        !early.status(),
        "the shield's guard reverts before the fill"
    );
    assert_eq!(execution_nonce(&destination_fork, account).await, nonce);
    destination_fork.revert(snapshot).await;

    // Another payload of the account took the shield's nonce, so the shield fails in the fill.
    // The message names no fallback recipient, so the fill reverts and the relayer keeps its
    // tokens.
    let snapshot = destination_fork.snapshot().await;
    destination_fork
        .set_storage(account, EXECUTION_NONCE_STORAGE_SLOT, nonce + U256::ONE)
        .await;
    let (failed, reverts) = fill(&destination_fork, spoke_pool, relay_of(&placed.deposit)).await;
    assert!(
        !failed.status() && reverts.is_some(),
        "a failed shield reverts a fill without a fallback recipient"
    );
    assert_eq!(
        (balance(RELAYER).await, balance(account).await),
        (funded, U256::ZERO)
    );
    destination_fork.revert(snapshot).await;

    // A plain fill of the same deposit to an address without code, on the same state.
    let snapshot = destination_fork.snapshot().await;
    let plain = ForkSpokePool::V3RelayData {
        recipient: address_to_bytes32(BRIDGE_RECEIVER),
        message: Bytes::new(),
        ..relay_of(&placed.deposit)
    };
    let (plain_fill, reverts) = fill(&destination_fork, spoke_pool, plain).await;
    assert!(plain_fill.status(), "the plain fill pays: {reverts:?}");
    destination_fork.revert(snapshot).await;

    let shielded_before = balance(railgun).await;
    let (filled, reverts) = fill(&destination_fork, spoke_pool, relay_of(&placed.deposit)).await;
    assert!(
        filled.status(),
        "the fill runs the handler's instructions and the shield: {reverts:?}"
    );
    // The fill carries the signed message's hash, which the wallet matches it by.
    let fills = filled
        .inner
        .logs()
        .iter()
        .filter(|log| log.address() == spoke_pool)
        .filter_map(|log| log.log_decode::<SpokePool::FilledRelay>().ok())
        .map(|log| log.inner.data)
        .collect::<Vec<_>>();
    let [relayed] = fills.as_slice() else {
        panic!("the SpokePool fills once");
    };
    assert_eq!(
        (
            relayed.recipient,
            Some(relayed.messageHash),
            Some(relayed.relayExecutionInfo.updatedMessageHash)
        ),
        (
            address_to_bytes32(handler),
            placed.terms.message_hash,
            placed.terms.message_hash
        )
    );
    // The output moves from the relayer through the handler and the account into Railgun,
    // less the shield fee, which goes to Railgun's treasury.
    let shields = shields_of(&filled, railgun, DESTINATION_TOKEN);
    let [(credited, fee)] = shields.as_slice() else {
        panic!("the fill shields once");
    };
    assert!(!fee.is_zero());
    assert_eq!(*credited + *fee, output);
    assert_eq!(balance(railgun).await, shielded_before + *credited);
    assert_eq!(
        (
            balance(RELAYER).await,
            balance(handler).await,
            balance(account).await
        ),
        (funded - output, held_by_handler, U256::ZERO)
    );
    assert_eq!(
        execution_nonce(&destination_fork, account).await,
        nonce + U256::ONE
    );

    let handler_gas = destination_fork
        .call_gas(
            filled.transaction_hash,
            handler,
            &MulticallHandler::handleV3AcrossMessageCall::SELECTOR,
        )
        .await;
    let shield_gas = destination_fork
        .call_gas(filled.transaction_hash, account, &placed.shield)
        .await;
    println!(
        "MEASURE destination_fill_gas handler_fill={} plain_fill={}",
        filled.gas_used, plain_fill.gas_used
    );
    println!(
        "MEASURE destination_fill_call_gas handler_message={handler_gas:?} account_shield={shield_gas:?}"
    );

    // The wallet verifies the delivery from the fill block's receipts, and the destination
    // account's record takes its payload's outcome from the swap.
    let block = BlockNumHash::new(filled.block_number.unwrap(), filled.block_hash.unwrap());
    let transaction_hash = filled.transaction_hash;
    assert_eq!(
        verify_fill(&wallet, &destination_fork, &placed, &filled).await,
        (
            Some(SwapBridgeOutcome::DeliveredVerified {
                block,
                transaction_hash,
                output_amount: output,
                shielded: true,
            }),
            Some(SwapDestinationOutcome::Shielded {
                block,
                transaction_hash,
            })
        )
    );

    // A second swap reuses both accounts. The fork wallet has no synced session, so the POI
    // verdict of the first shield's note is stubbed here: the note is stated as accepted on
    // every active list. The delegations, nonces, balances, settlement, fill and shield are
    // the forks' own.
    let first = use_history(&wallet, &placed);
    let source = placed.swap.executor;
    let source_nonce = execution_nonce(&fork, source).await;
    let notes = AcceptedShieldNotes(vec![accepted_shield_note(&wallet, &filled, *credited)]);
    let reused = wallet
        .reuse_private(&fork, &destination_fork, &placed, notes)
        .await;
    assert_eq!(use_history(&wallet, &placed), first);
    let swap = wallet.swap_from(&fork, reused).await;
    let settled = wallet.settle_deposit(&fork, swap).await;
    let again = placed_private_deposit(&wallet, settled, BridgeShieldFailure::RefundOnOrigin);
    let second_use = again.swap.swap_use;
    assert_ne!(second_use, placed.swap.swap_use);

    // Each account signed at its current nonce, past every nonce of the first swap, and
    // neither signed another setup.
    let source_record = wallet.record(again.swap.operation);
    assert_eq!(
        order_of(&source_record, again.swap.uid).use_id(),
        Some(second_use)
    );
    let setup_nonce = source_record.issued()[0].nonce();
    assert_eq!(source_nonce, setup_nonce + U256::from(3));
    assert_eq!(
        issued_nonces(&source_record),
        [
            (ExecutorPayloadPurpose::Operation, setup_nonce),
            (ExecutorPayloadPurpose::SwapPreHook, setup_nonce + U256::ONE),
            (
                ExecutorPayloadPurpose::SwapPostHook,
                setup_nonce + U256::from(2)
            ),
            (ExecutorPayloadPurpose::SwapPreHook, source_nonce),
            (
                ExecutorPayloadPurpose::SwapPostHook,
                source_nonce + U256::ONE
            ),
        ]
    );
    assert_eq!(
        execution_nonce(&fork, source).await,
        source_nonce + U256::from(2)
    );
    assert_eq!(
        issued_nonces(&wallet.destination_record(again.account.operation())),
        [
            (ExecutorPayloadPurpose::Operation, nonce - U256::ONE),
            (ExecutorPayloadPurpose::SwapDestinationShield, nonce),
            (
                ExecutorPayloadPurpose::SwapDestinationShield,
                nonce + U256::ONE
            ),
        ]
    );
    assert_eq!(again.account.observed().nonce(), nonce + U256::ONE);
    assert_eq!(
        execution_nonce(&destination_fork, account).await,
        nonce + U256::ONE
    );

    // The second fill moves its output through the handler and the same account into Railgun,
    // less the shield fee, and uses the account's next nonce.
    assert!(
        destination_fork.timestamp().await < u64::from(again.terms.fill_deadline),
        "the destination fork's clock is before the second fill deadline"
    );
    let (relayer_before, shielded_before) = (balance(RELAYER).await, balance(railgun).await);
    let (refilled, reverts) = fill(&destination_fork, spoke_pool, relay_of(&again.deposit)).await;
    assert!(
        refilled.status(),
        "the second fill runs the reused account's shield: {reverts:?}"
    );
    let reshields = shields_of(&refilled, railgun, DESTINATION_TOKEN);
    let [(recredited, refee)] = reshields.as_slice() else {
        panic!("the second fill shields once");
    };
    assert!(!refee.is_zero());
    assert_eq!(*recredited + *refee, output);
    assert_eq!(balance(railgun).await, shielded_before + *recredited);
    assert_eq!(
        (
            balance(RELAYER).await,
            balance(handler).await,
            balance(account).await
        ),
        (relayer_before - output, held_by_handler, U256::ZERO)
    );
    assert_eq!(
        execution_nonce(&destination_fork, account).await,
        nonce + U256::from(2)
    );

    // The wallet verifies the second delivery and records it under the second use. The first
    // use keeps its own outcome, and the first swap's order and uses are what they were.
    let reblock = BlockNumHash::new(refilled.block_number.unwrap(), refilled.block_hash.unwrap());
    let redelivered = SwapDestinationOutcome::Shielded {
        block: reblock,
        transaction_hash: refilled.transaction_hash,
    };
    assert_eq!(
        verify_fill(&wallet, &destination_fork, &again, &refilled).await,
        (
            Some(SwapBridgeOutcome::DeliveredVerified {
                block: reblock,
                transaction_hash: refilled.transaction_hash,
                output_amount: output,
                shielded: true,
            }),
            Some(redelivered)
        )
    );
    let destination_record = wallet.destination_record(again.account.operation());
    let outcome_of = |swap_use| {
        destination_record
            .swap_destination_use(swap_use)
            .and_then(|served| served.outcome)
    };
    assert_eq!(
        (outcome_of(placed.swap.swap_use), outcome_of(second_use)),
        (
            Some(SwapDestinationOutcome::Shielded {
                block,
                transaction_hash,
            }),
            Some(redelivered)
        )
    );
    assert_eq!(use_history(&wallet, &placed), first);

    // The first swap's payloads stay public and signed, but their nonces are consumed. Its
    // hooks, replayed on the swap's chain, revert and move nothing.
    let held = (
        fork.erc20_balance(WETH, source).await,
        fork.erc20_balance(USDC, source).await,
    );
    for hook in placed.swap.hooks.pre.iter().chain(&placed.swap.hooks.post) {
        let replayed = fork
            .send_as_bypass(hook.target, hook.call_data.clone(), 3_000_000)
            .await;
        assert!(!replayed.status(), "a hook at a consumed nonce reverts");
    }
    assert_eq!(
        execution_nonce(&fork, source).await,
        source_nonce + U256::from(2)
    );
    assert_eq!(
        (
            fork.erc20_balance(WETH, source).await,
            fork.erc20_balance(USDC, source).await
        ),
        held
    );
    // Its shield, replayed on the destination chain while the account holds enough to pass
    // the guard, reverts on its nonce alone and leaves the tokens in the account.
    destination_fork
        .add_erc20(DESTINATION_TOKEN, account, output)
        .await;
    let shielded = balance(railgun).await;
    let replayed = destination_fork
        .send_as_bypass(account, placed.shield.clone(), FILL_GAS)
        .await;
    assert!(!replayed.status(), "a shield at a consumed nonce reverts");
    assert_eq!(
        (
            execution_nonce(&destination_fork, account).await,
            balance(account).await,
            balance(railgun).await
        ),
        (nonce + U256::from(2), output, shielded)
    );
    wallet.finish().await;
}

// With the destination stealth account as fallback recipient, a fill whose shield fails still
// completes: the handler reverts its instructions and passes the tokens to the account, which
// holds them for recovery there.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_private_across_fill_with_a_failed_shield_is_held_by_the_destination_account() {
    let fork = ForkChain::start().await;
    let destination_fork = ForkChain::start_destination(DESTINATION_CHAIN).await;
    let mut wallet = Wallet::open(&fork).with_destination(&destination_fork);
    let placed = private_deposit(
        &mut wallet,
        &fork,
        &destination_fork,
        BridgeSurplus::KeepInAccount,
        BridgeShieldFailure::KeepOnDestination,
    )
    .await;
    let destination = wallet.destination.as_ref().unwrap();
    let spoke_pool = destination.chain.bridge_profile().unwrap().spoke_pool();
    let railgun = destination
        .chain
        .require_railgun()
        .unwrap()
        .deployment
        .contract;
    let account = placed.account.executor();
    let output = U256::from(DESTINATION_MINIMUM);
    let balance = |holder| destination_fork.erc20_balance(DESTINATION_TOKEN, holder);
    fund_relayer(&destination_fork, spoke_pool).await;

    // Another payload of the account took the shield's nonce, so the shield fails in the fill.
    let taken = execution_nonce(&destination_fork, account).await + U256::ONE;
    destination_fork
        .set_storage(account, EXECUTION_NONCE_STORAGE_SLOT, taken)
        .await;
    let shielded_before = balance(railgun).await;
    let (filled, reverts) = fill(&destination_fork, spoke_pool, relay_of(&placed.deposit)).await;
    assert!(
        filled.status(),
        "the handler passes a failed shield's tokens to the fallback recipient: {reverts:?}"
    );
    assert_eq!(
        (balance(account).await, balance(railgun).await),
        (output, shielded_before)
    );
    assert!(shields_of(&filled, railgun, DESTINATION_TOKEN).is_empty());
    assert_eq!(execution_nonce(&destination_fork, account).await, taken);

    let block = BlockNumHash::new(filled.block_number.unwrap(), filled.block_hash.unwrap());
    let transaction_hash = filled.transaction_hash;
    assert_eq!(
        verify_fill(&wallet, &destination_fork, &placed, &filled).await,
        (
            Some(SwapBridgeOutcome::HeldOnDestination {
                block,
                transaction_hash,
                amount: output,
            }),
            Some(SwapDestinationOutcome::Held {
                block,
                transaction_hash,
            })
        )
    );
    wallet.finish().await;
}

/// Intrinsic gas of `data` as transaction calldata.
fn calldata_gas(data: &[u8]) -> u64 {
    data.iter()
        .map(|byte| if *byte == 0 { 4 } else { 16 })
        .sum()
}

// The handler message makes a private Across post-hook larger and its `FundsDeposited` event
// longer than a plain one's. The post-hook still deposits within the gas limit its plan declares,
// for either surplus choice.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_private_across_post_hook_deposits_within_its_gas_limit() {
    let fork = ForkChain::start().await;
    let destination_fork = ForkChain::start_destination(DESTINATION_CHAIN).await;
    let mut wallet = Wallet::open(&fork).with_destination(&destination_fork);
    for (surplus, label) in SURPLUS_CHOICES {
        let (plain, plain_receipt, terms) = wallet.settle_across(&fork, surplus).await;
        assert_eq!(deposits(&plain_receipt, terms.spoke_pool).len(), 1);
        let placed = private_deposit(
            &mut wallet,
            &fork,
            &destination_fork,
            surplus,
            BridgeShieldFailure::RefundOnOrigin,
        )
        .await;
        let (plain_hook, private_hook) = (&plain.hooks.post[0], &placed.swap.hooks.post[0]);
        let plain_gas = fork
            .call_gas(
                plain_receipt.transaction_hash,
                plain.executor,
                &plain_hook.call_data,
            )
            .await;
        let private_gas = fork
            .call_gas(
                placed.receipt.transaction_hash,
                placed.swap.executor,
                &private_hook.call_data,
            )
            .await;
        println!(
            "MEASURE across_post_hook_gas surplus={label} plain={plain_gas:?} private={private_gas:?}"
        );
        println!(
            "MEASURE across_post_hook_gas_limit surplus={label} plain={} private={}",
            plain_hook.gas_limit, private_hook.gas_limit
        );
        println!(
            "MEASURE across_post_hook_calldata_gas surplus={label} plain={} private={}",
            calldata_gas(&plain_hook.call_data),
            calldata_gas(&private_hook.call_data)
        );
        println!(
            "MEASURE across_settlement_gas surplus={label} plain={} private={}",
            plain_receipt.gas_used, placed.receipt.gas_used
        );
    }
    wallet.finish().await;
}

// Ethereum's USDT refuses a change from one nonzero allowance to another. A Public account
// that already allows the `SpokePool` 100 and sells 200 resets the allowance first, and both
// approvals cost no more gas than the plan the review showed.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_public_usdt_approval_resets_a_short_allowance_first() {
    use alloy::primitives::{address, keccak256};
    use broadcaster_core::contracts::shield::build_approve_calldata;

    const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    const MAX_FEE_PER_GAS: u128 = 200_000_000_000;
    const MAX_PRIORITY_FEE_PER_GAS: u128 = 2_000_000_000;

    let fork = ForkChain::start().await;
    // The swap's record is on the destination chain's owner, which reads nothing of its own
    // chain here. Every read and transaction goes to the origin fork.
    let wallet = Wallet::open(&fork).with_destination(&fork);
    let origin = &wallet.chain;
    let owner = &wallet.destination.as_ref().unwrap().owner;
    let spoke_pool = origin.bridge_origin_profile().unwrap().spoke_pool();
    let key = keccak256(format!(
        "{:?} {}",
        std::time::SystemTime::now(),
        std::process::id()
    ));
    let signer = VaultedPublicSigner::Software(SoftwareEvmSigner::from_private_key(key.0).unwrap());
    let source = signer.address();
    let (sell_amount, allowed) = (U256::from(200), U256::from(100));
    let allowance = || ForkAllowance::allowanceCall {
        owner: source,
        spender: spoke_pool,
    };

    // The account holds USDT and gas money, and already allows the pool less than it sells.
    fork.impersonate(source).await;
    fork.add_erc20(USDT, source, U256::from(1_000)).await;
    let receipt = fork
        .send(
            TransactionRequest::default()
                .from(source)
                .to(USDT)
                .input(Bytes::from(build_approve_calldata(spoke_pool, allowed)).into())
                .gas_limit(100_000),
        )
        .await;
    assert!(receipt.status());
    assert_eq!(fork.call(USDT, allowance()).await, allowed);

    let plan = owner
        .plan_public_swap_gas(
            origin,
            source,
            USDT,
            spoke_pool,
            sell_amount,
            true,
            MAX_FEE_PER_GAS,
            MAX_PRIORITY_FEE_PER_GAS,
        )
        .await
        .unwrap();
    assert_eq!(plan.approval_gas_limits.len(), 2);

    // The smallest claim of a direct USDT deposit from this account, approved for the plan.
    let (operation, id) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    let mut bounds = setup_approval(USDT, WETH, SwapDelivery::Reshield).bounds;
    bounds.sell_amount = sell_amount;
    bounds.destination_minimum = Some(U256::from(100));
    bounds.destination_shield_fee_bps = Some(crate::RAILGUN_PROTOCOL_FEE_BPS);
    bounds.destination_setup_fee = Some(U256::from(1_000));
    owner
        .claim_public_swap(PublicSwapUseClaim {
            id,
            origin_chain: 1,
            source,
            source_scope: PublicAccountScope::PrivateWallet {
                wallet_uuid: TEST_WALLET_ID.to_owned(),
            },
            account: SwapAccountChoice::New(operation),
            destination_token: DESTINATION_TOKEN,
            intent: PublicSwapIntent {
                bridged_token: USDT,
                order: false,
            },
            approval: PublicSwapApproval {
                bounds,
                price_verified: Some(true),
                price_acknowledged: false,
                sell_token: USDT,
                on_shield_failure: BridgeShieldFailure::KeepOnDestination,
                destination: SwapApprovedAccount {
                    address: None,
                    setup: true,
                },
                max_gas_cost: plan.max_gas_cost,
            },
        })
        .unwrap();

    let funded = fork.native_balance(source).await;
    owner
        .submit_public_swap_approvals_with_signer(
            operation,
            id,
            origin,
            &signer,
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: MAX_FEE_PER_GAS,
                max_priority_fee_per_gas: MAX_PRIORITY_FEE_PER_GAS,
            },
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(fork.call(USDT, allowance()).await, sell_amount);

    // approve(0), then approve(200), both included and successful.
    let store =
        ExecutorStore::new(wallet.db.clone(), wallet.view.clone(), DESTINATION_CHAIN).unwrap();
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    let (_, swap) = record.public_swap_use(id).unwrap();
    let [reset, approval] = swap.transactions() else {
        panic!("a reset and an approval were sent");
    };
    for (transaction, kind, value) in [
        (reset, PublicSwapTransactionKind::ApprovalReset, U256::ZERO),
        (approval, PublicSwapTransactionKind::Approval, sell_amount),
    ] {
        assert_eq!(transaction.kind, kind);
        assert_eq!(
            transaction.transaction.input.input().unwrap()[..],
            build_approve_calldata(spoke_pool, value)[..]
        );
        assert!(transaction.inclusion.unwrap().succeeded);
    }

    // The approvals carry no value, so what the account lost is the gas both receipts paid.
    let paid = funded - fork.native_balance(source).await;
    assert!(!paid.is_zero());
    assert!(
        paid <= plan.max_gas_cost,
        "the approvals paid {paid} wei of the planned {} wei",
        plan.max_gas_cost
    );
    println!(
        "MEASURE public_usdt_approvals_gas paid={paid} planned={}",
        plan.max_gas_cost
    );
    drop(store);
    wallet.finish().await;
}

/// The fee every Public account's transaction here is reviewed and sent at.
const PUBLIC_MAX_FEE_PER_GAS: u128 = 200_000_000_000;
const PUBLIC_MAX_PRIORITY_FEE_PER_GAS: u128 = 2_000_000_000;
const PUBLIC_GAS_FEE: PublicActionGasFeeSelection = PublicActionGasFeeSelection::Custom {
    max_fee_per_gas: PUBLIC_MAX_FEE_PER_GAS,
    max_priority_fee_per_gas: PUBLIC_MAX_PRIORITY_FEE_PER_GAS,
};
/// What a Public account's direct deposit sells here, in USDC base units.
const PUBLIC_DEPOSIT: u64 = 20_000_000;
/// The approved minimum on Polygon of that deposit, in USDC base units.
const PUBLIC_DEPOSIT_MINIMUM: u64 = 19_900_000;
/// The buy amount of every Public account's order here, in USDC base units.
const PUBLIC_ORDER_BUY_AMOUNT: u64 = 15_000_000;
/// How long a Public account's order and its hook batch stay valid, and how long a direct
/// deposit's quote is taken for, in seconds. It is within the ten minutes the wallet signs for.
const PUBLIC_VALID_SECS: u64 = 9 * 60;
/// How far ahead of the swap chain's clock the quote of a deposit that must fail is dated, in
/// seconds. The `SpokePool` refuses a deposit whose block precedes its quote.
const QUOTE_AHEAD_SECS: u64 = 3 * 60;
/// Gas limit of a hook batch sent to the factory on its own, above a batch that deploys its
/// proxy and deposits.
const BATCH_GAS: u64 = 1_500_000;
/// The gas limit an order's invalidation is reviewed with.
const INVALIDATION_GAS_LIMIT: u64 = 100_000;
/// Polygon's WETH, which Across pays out for WETH deposited on Ethereum.
const POLYGON_WETH: Address =
    alloy::primitives::address!("7ceb23fd6bc0add59e62ac25578270cff1b9f619");
const BNB_CHAIN: u64 = 56;
/// BNB Chain's USDC, which has 18 decimals.
const BNB_USDC: Address = alloy::primitives::address!("8ac76a51cc950d9822d68b83fe1ad97b32cd580d");

/// What a swap paid from a Public account on Ethereum sells, deposits and must deliver.
#[derive(Clone, Copy)]
struct PublicTerms {
    /// `Address::ZERO` for ETH.
    sell_token: Address,
    sell_amount: U256,
    /// The token deposited into Across: the sold token, WETH for ETH, or an order's buy token.
    bridged_token: Address,
    /// The amount the deposit is quoted for: the sold amount, or an order's buy amount.
    bridged_amount: U256,
    destination_token: Address,
    destination_minimum: U256,
    /// The hook gas limit an order was reviewed with. `None` for a direct deposit.
    hook_gas_limit: Option<u64>,
}

/// A claimed swap paid from a Public account, whose new destination stealth account is set up
/// and confirmed on the destination fork.
struct PublicSwap {
    operation: ExecutorOperationId,
    id: SwapUseId,
    /// The Public account.
    source: Address,
    terms: PublicTerms,
    /// The account's own transactions as planned for the approval.
    plan: PublicSwapGasPlan,
    /// The destination stealth account.
    account: DelegatedSwapExecutor,
}

/// A swap's signed delivery: the destination account's shield, the handler message that runs
/// it, and the deposit terms Across quoted for that message.
struct SignedDelivery {
    delivery: AcrossPrivateDelivery,
    message: Bytes,
    terms: AcrossOrderTerms,
}

/// An orderbook stub for one Public account's order. It records each order request's body, and
/// answers a read of the order's trades with the block last set, or with no trade.
struct PublicOrderbook {
    client: CowOrderbookClient,
    orders: Arc<Mutex<Vec<Value>>>,
    trade_block: Arc<Mutex<Option<u64>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PublicOrderbook {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A Public account's order placed through the wallet, as the orderbook stub received it.
struct PlacedOrder {
    swap: PublicSwap,
    signed: SignedDelivery,
    orderbook: PublicOrderbook,
    order: Order,
    uid: OrderUid,
    signature: Bytes,
    /// The order's one post-hook: the signed `executeHooks` call on the cow-shed factory.
    hook: AppDataHook,
    /// The Public account's cow-shed proxy, the order's receiver.
    proxy: Address,
    /// The order's `validTo`, also its hook batch's deadline.
    valid_to: u32,
}

/// Where a Public account's order from Ethereum delivers, and how much of the scenario runs.
#[derive(Clone, Copy)]
struct OrderDestination {
    chain_id: u64,
    /// The environment variable that names the RPC the chain is forked from.
    rpc_env: &'static str,
    /// The chain's USDC.
    token: Address,
    /// The approved minimum of `token` for the order's buy amount, in `token`'s base units.
    minimum: u128,
    /// Whether the failed hook, the late batch run and the cancellation run after the delivery.
    every_outcome: bool,
}

/// A new software key of a Public account. `label` tells one test's accounts apart.
fn public_signer(label: &str) -> VaultedPublicSigner {
    let key = alloy::primitives::keccak256(format!(
        "{label} {:?} {}",
        std::time::SystemTime::now(),
        std::process::id()
    ));
    VaultedPublicSigner::Software(SoftwareEvmSigner::from_private_key(key.0).unwrap())
}

/// Unix seconds `secs` from now, for an order's `validTo` or a deposit's deadline.
fn valid_until(secs: u64) -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    u32::try_from(now + secs).unwrap()
}

/// An Across client over the local stub at `url`.
fn across_client(url: url::Url) -> crate::bridge::AcrossClient {
    crate::bridge::AcrossClient::new(
        OperationHttpClient::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        ),
        url,
    )
    .unwrap()
}

/// Across's record of a deposit that pays `recipient` at least `output` on `destination_chain`:
/// its `status`, the fill it locates and the refund transaction it names.
fn across_deposit_record(
    status: &str,
    fill: Option<&TransactionReceipt>,
    refund: Option<B256>,
    recipient: Address,
    destination_chain: u64,
    output: U256,
) -> String {
    json!({"deposit": {
        "status": status,
        "fillBlockNumber": fill.map(|fill| fill.block_number.unwrap()),
        "fillTx": fill.map(|fill| fill.transaction_hash),
        "depositRefundTxHash": refund,
        "outputAmount": output.to_string(),
        "recipient": recipient,
        "destinationChainId": destination_chain.to_string()
    }})
    .to_string()
}

/// An Across client whose deposit lookups a local stub answers with what the returned reply
/// holds, at first `reply`.
async fn across_deposit_stub(
    reply: String,
) -> (
    crate::bridge::AcrossClient,
    Arc<Mutex<String>>,
    tokio::task::JoinHandle<()>,
) {
    let reply = Arc::new(Mutex::new(reply));
    let served = reply.clone();
    let (url, _, task) = spawn_bridge_stub(move |_| served.lock().unwrap().clone()).await;
    (across_client(url), reply, task)
}

/// An orderbook stub on the chain `chain_id` for a Public account's order at `settlement`.
async fn spawn_public_orderbook(chain_id: u64, settlement: Address) -> PublicOrderbook {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mainnet", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let orders = Arc::new(Mutex::new(Vec::new()));
    let trade_block = Arc::new(Mutex::new(None));
    let (recorded, reported) = (orders.clone(), trade_block.clone());
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
            // The only read is of the order's trades, by its UID.
            let (status, reply) = if request_line.starts_with("GET") {
                let block: Option<u64> = *reported.lock().unwrap();
                let trades =
                    block.map_or_else(|| json!([]), |block| json!([{"blockNumber": block}]));
                ("200 OK", trades.to_string())
            } else {
                let owner = body["from"].as_str().unwrap().parse().unwrap();
                let uid = order_uid(&submitted_order(&body), chain_id, settlement, owner);
                recorded.lock().unwrap().push(body);
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
        chain_id,
    )
    .unwrap();
    PublicOrderbook {
        client,
        orders,
        trade_block,
        task,
    }
}

/// The claim of a swap with `terms` paid from the Public account `source` on Ethereum, for the
/// new destination account `operation`. A failed shield reverts the fill, so that Across
/// refunds the deposit.
fn public_claim(
    id: SwapUseId,
    operation: ExecutorOperationId,
    source: Address,
    terms: PublicTerms,
    max_gas_cost: U256,
) -> PublicSwapUseClaim {
    let mut bounds = setup_approval(
        terms.sell_token,
        terms.bridged_token,
        SwapDelivery::Reshield,
    )
    .bounds;
    bounds.sell_amount = terms.sell_amount;
    bounds.buy_amount = terms.bridged_amount;
    bounds.post_hook_gas_limit = terms.hook_gas_limit;
    bounds.destination_minimum = Some(terms.destination_minimum);
    bounds.destination_shield_fee_bps = Some(crate::RAILGUN_PROTOCOL_FEE_BPS);
    bounds.destination_setup_fee = Some(U256::from(1_000));
    PublicSwapUseClaim {
        id,
        origin_chain: 1,
        source,
        source_scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: TEST_WALLET_ID.to_owned(),
        },
        account: SwapAccountChoice::New(operation),
        destination_token: terms.destination_token,
        intent: PublicSwapIntent {
            bridged_token: terms.bridged_token,
            order: terms.hook_gas_limit.is_some(),
        },
        approval: PublicSwapApproval {
            bounds,
            price_verified: Some(true),
            price_acknowledged: false,
            sell_token: terms.sell_token,
            on_shield_failure: BridgeShieldFailure::RefundOnOrigin,
            destination: SwapApprovedAccount {
                address: None,
                setup: true,
            },
            max_gas_cost,
        },
    }
}

impl Wallet {
    /// Plan the gas of the Public account `signer`'s own transactions for a swap with `terms`,
    /// claim the swap for a new destination stealth account, deliver that account's
    /// delegation-only setup on `destination_fork`, and confirm it delegated there.
    async fn public_swap(
        &self,
        destination_fork: &ForkChain,
        signer: &VaultedPublicSigner,
        terms: PublicTerms,
    ) -> PublicSwap {
        let destination = self.destination.as_ref().unwrap();
        let (origin, owner) = (&self.chain, &destination.owner);
        let source = signer.address();
        let order = terms.hook_gas_limit.is_some();
        // An order's Sell token is pulled by `CoW`'s vault relayer, a deposit's by the pool.
        let spender = if order {
            origin.public_swap_profile().unwrap().vault_relayer()
        } else {
            origin.bridge_origin_profile().unwrap().spoke_pool()
        };
        let plan = owner
            .plan_public_swap_gas(
                origin,
                source,
                terms.sell_token,
                spender,
                terms.sell_amount,
                !order,
                PUBLIC_MAX_FEE_PER_GAS,
                PUBLIC_MAX_PRIORITY_FEE_PER_GAS,
            )
            .await
            .unwrap();
        let (operation, id) = (
            ExecutorOperationId::random().unwrap(),
            SwapUseId::random().unwrap(),
        );
        let claim = public_claim(id, operation, source, terms, plan.max_gas_cost);
        self.claimed_public_swap(destination_fork, operation, claim, terms, plan)
            .await
    }

    /// Claim a swap with `terms` for the new destination stealth account `operation` with
    /// `claim`, whose approval was priced with `plan`, deliver that account's delegation-only
    /// setup on `destination_fork`, and confirm it delegated there.
    async fn claimed_public_swap(
        &self,
        destination_fork: &ForkChain,
        operation: ExecutorOperationId,
        claim: PublicSwapUseClaim,
        terms: PublicTerms,
        plan: PublicSwapGasPlan,
    ) -> PublicSwap {
        let destination = self.destination.as_ref().unwrap();
        let owner = &destination.owner;
        let (id, source) = (claim.id, claim.source);
        owner.claim_public_swap(claim).unwrap();

        let authorization = password();
        let delegate = destination
            .chain
            .accepted_executor_profile()
            .unwrap()
            .delegate();
        let mut candidate = broadcaster(delegate);
        candidate.chain_id = destination.chain.chain_id;
        let SwapPairSide::Setup(prepared) = owner
            .prepare_public_swap_destination(operation, id, Some(candidate), &authorization)
            .await
            .unwrap()
        else {
            panic!("a new destination account needs setup");
        };
        let installed = self
            .install(
                destination_fork,
                owner,
                &destination.chain,
                &prepared,
                &authorization,
            )
            .await;
        let confirmed = destination_fork.block_number().await - destination.chain.finality_depth;
        let account = owner
            .delegated_public_swap_destination(operation, confirmed, id, None)
            .await
            .unwrap();
        assert_eq!(
            (account.executor(), account.observed().nonce()),
            (installed.executor(), installed.observed().nonce())
        );
        PublicSwap {
            operation,
            id,
            source,
            terms,
            plan,
            account,
        }
    }

    /// Sign `swap`'s delivery for a deposit that can be made until `valid_to`. The destination
    /// account signs its shield, and a local Across stub quotes the approved destination
    /// minimum for the handler message that runs it, dated `quoted` on the swap chain's clock.
    async fn sign_public_delivery(
        &self,
        swap: &PublicSwap,
        quoted: u64,
        valid_to: u32,
    ) -> SignedDelivery {
        let destination = self.destination.as_ref().unwrap();
        let spoke_pool = self.chain.bridge_origin_profile().unwrap().spoke_pool();
        let destination_spoke_pool = destination.chain.bridge_profile().unwrap().spoke_pool();
        let output = swap.terms.destination_minimum;
        let (url, _, stub) = spawn_bridge_stub(move |_| {
            across_quote_at(spoke_pool, destination_spoke_pool, output, quoted)
        })
        .await;
        let signed = destination
            .owner
            .sign_public_swap_delivery(PublicSwapDeliverySigning {
                operation: swap.operation,
                swap_use: swap.id,
                delegated: swap.account,
                origin: &self.chain,
                across: &across_client(url),
                input_amount: swap.terms.bridged_amount,
                valid_to,
                authorization: &password(),
                notes: None,
            })
            .await
            .unwrap();
        stub.abort();
        let PublicSwapDelivery::Signed {
            delivery,
            message,
            terms,
        } = signed
        else {
            panic!("Across's quote covers the approved destination minimum");
        };
        SignedDelivery {
            delivery,
            message,
            terms,
        }
    }
}

/// The swap of `swap`'s use as its destination account's record holds it, and what became of
/// that account's shield.
fn public_swap_record(
    wallet: &Wallet,
    swap: &PublicSwap,
) -> (PublicSwapRecord, Option<SwapDestinationOutcome>) {
    let record = wallet.destination_record(swap.operation);
    let Some(SwapUseRole::PublicSourceDestination {
        swap: saved,
        outcome,
        ..
    }) = record.swap_use(swap.id).map(SwapUseRecord::role)
    else {
        panic!("the use delivers a swap paid from a Public account");
    };
    ((**saved).clone(), *outcome)
}

/// The state of `swap`'s order, from its record.
fn public_order_state(wallet: &Wallet, swap: &PublicSwap) -> Option<PublicSwapOrderState> {
    public_swap_order_state(&public_swap_record(wallet, swap).0)
}

/// Mine `fork` until its clock is past `time`, Unix seconds.
async fn mine_past(fork: &ForkChain, time: u64) {
    loop {
        let now = fork.timestamp().await;
        if now > time {
            return;
        }
        fork.increase_time(time + 1 - now).await;
        fork.mine(1).await;
    }
}

/// Send `swap`'s deposit from its Public account through the wallet. The pinned `SpokePool`
/// must take one deposit of the signed amounts that names the Public account as depositor and
/// the destination chain's handler as recipient, with the signed message, for no more gas than
/// the plan the swap was approved with. Returns the deposit's receipt and event.
async fn deposit_from_public_account(
    wallet: &Wallet,
    fork: &ForkChain,
    signer: &VaultedPublicSigner,
    swap: &PublicSwap,
    signed: &SignedDelivery,
) -> (TransactionReceipt, SpokePool::FundsDeposited) {
    let destination = wallet.destination.as_ref().unwrap();
    let origin = &wallet.chain;
    let outcome = destination
        .owner
        .submit_public_swap_deposit_with_signer(
            swap.operation,
            swap.id,
            origin,
            signer,
            PUBLIC_GAS_FEE,
            &signed.delivery,
            &signed.terms,
            &mut |_| {},
        )
        .await
        .unwrap();
    let PublicSwapTransactionOutcome::Included {
        transaction_hash, ..
    } = outcome
    else {
        panic!("the deposit succeeds: {outcome:?}");
    };
    let receipt = fork.receipt(transaction_hash).await;
    let deposits = deposits(
        &receipt,
        origin.bridge_origin_profile().unwrap().spoke_pool(),
    );
    let [deposit] = deposits.as_slice() else {
        panic!("the Public account deposits once");
    };
    let handler = destination
        .chain
        .bridge_profile()
        .unwrap()
        .multicall_handler();
    assert_eq!(
        (deposit.depositor, deposit.recipient),
        (address_to_bytes32(swap.source), address_to_bytes32(handler))
    );
    assert_eq!(deposit.message, signed.message);
    assert_eq!(
        (
            deposit.inputToken,
            deposit.outputToken,
            deposit.destinationChainId
        ),
        (
            address_to_bytes32(swap.terms.bridged_token),
            address_to_bytes32(swap.terms.destination_token),
            U256::from(destination.chain.chain_id)
        )
    );
    assert_eq!(
        (deposit.inputAmount, deposit.outputAmount),
        (swap.terms.sell_amount, swap.terms.destination_minimum)
    );
    assert_eq!(
        (signed.terms.input_amount, signed.terms.output_amount),
        (deposit.inputAmount, deposit.outputAmount)
    );
    assert_eq!(
        (deposit.quoteTimestamp, deposit.fillDeadline),
        (signed.terms.quote_timestamp, signed.terms.fill_deadline)
    );
    let limit = swap.plan.deposit_gas_limit.unwrap();
    assert!(
        receipt.gas_used <= limit,
        "the deposit used {} gas of the planned {limit}",
        receipt.gas_used
    );
    println!(
        "MEASURE public_deposit_gas sell_token={} used={} limit={limit}",
        swap.terms.sell_token, receipt.gas_used
    );
    let deposit = deposit.clone();
    (receipt, deposit)
}

/// Read `swap`'s hand-off through the wallet once the block of `receipt`, its deposit's, is
/// final on the swap's chain. The wallet must record that transaction as the hand-off of
/// `deposit`, with the amounts its event carries.
async fn observe_public_handoff(
    wallet: &Wallet,
    fork: &ForkChain,
    swap: &PublicSwap,
    receipt: &TransactionReceipt,
    deposit: &SpokePool::FundsDeposited,
) {
    let origin = &wallet.chain;
    let owner = &wallet.destination.as_ref().unwrap().owner;
    let observe = || owner.observe_public_swap_handoff(swap.operation, swap.id, origin);
    assert_eq!(
        observe().await.unwrap(),
        None,
        "the deposit's block isn't final yet"
    );
    fork.mine(origin.finality_depth).await;
    let handoff = observe()
        .await
        .unwrap()
        .expect("the final deposit is the swap's hand-off");
    assert_eq!(
        handoff,
        SwapBridgeHandoff {
            observation: SwapObservation {
                block: BlockNumHash::new(
                    receipt.block_number.unwrap(),
                    receipt.block_hash.unwrap()
                ),
                transaction_hash: Some(receipt.transaction_hash),
            },
            deposit_id: Some(deposit.depositId),
        }
    );
    let observed = public_swap_record(wallet, swap).0.observations();
    assert_eq!(observed.bridge_handoff, Some(handoff));
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: deposit.inputAmount,
            output_amount: deposit.outputAmount,
        })
    );
}

/// Fill `deposit` on the destination fork as the relayer. The handler must pass the deposit's
/// whole output of `token` to the destination stealth account `account`, whose shield must put
/// all of it into Railgun, less the shield fee, at the account's next nonce. Returns the fill.
async fn fill_and_shield(
    destination_fork: &ForkChain,
    destination: &Destination,
    token: Address,
    deposit: &SpokePool::FundsDeposited,
    account: DelegatedSwapExecutor,
) -> TransactionReceipt {
    let spoke_pool = destination.chain.bridge_profile().unwrap().spoke_pool();
    let railgun = destination
        .chain
        .require_railgun()
        .unwrap()
        .deployment
        .contract;
    let executor = account.executor();
    let nonce = execution_nonce(destination_fork, executor).await;
    assert_eq!(nonce, account.observed().nonce());
    assert!(
        destination_fork.timestamp().await < u64::from(deposit.fillDeadline),
        "the destination fork's clock is before the fill deadline"
    );
    let (filled, reverts) = fill_on(
        destination_fork,
        spoke_pool,
        relay_of(deposit),
        destination.chain.chain_id,
    )
    .await;
    assert!(
        filled.status(),
        "the fill runs the handler's instructions and the shield: {reverts:?}"
    );
    let shields = shields_of(&filled, railgun, token);
    let [(credited, fee)] = shields.as_slice() else {
        panic!("the fill shields once");
    };
    assert!(!fee.is_zero());
    assert_eq!(*credited + *fee, deposit.outputAmount);
    assert_eq!(
        destination_fork.erc20_balance(token, executor).await,
        U256::ZERO
    );
    assert_eq!(
        execution_nonce(destination_fork, executor).await,
        nonce + U256::ONE
    );
    filled
}

/// The wallet's explicit status check of `swap`'s deposit once `filled`'s block is final on the
/// destination fork: one lookup at an Across stub whose record names that fill, then the read
/// of its block's receipts. It must record the delivery of `output`, the deposit's whole
/// output, as verified and shielded, and the destination account's shield as run.
async fn verify_public_delivery(
    wallet: &Wallet,
    destination_fork: &ForkChain,
    swap: &PublicSwap,
    output: U256,
    filled: &TransactionReceipt,
) {
    let destination = wallet.destination.as_ref().unwrap();
    destination_fork
        .mine(destination.chain.finality_depth)
        .await;
    let handler = destination
        .chain
        .bridge_profile()
        .unwrap()
        .multicall_handler();
    let located = across_deposit_record(
        "filled",
        Some(filled),
        None,
        handler,
        destination.chain.chain_id,
        output,
    );
    let (across, _, stub) = across_deposit_stub(located).await;
    let outcome = destination
        .owner
        .check_public_swap_bridge(swap.operation, swap.id, &across)
        .await
        .unwrap();
    stub.abort();
    let block = BlockNumHash::new(filled.block_number.unwrap(), filled.block_hash.unwrap());
    let transaction_hash = filled.transaction_hash;
    assert_eq!(
        outcome,
        Some(SwapBridgeOutcome::DeliveredVerified {
            block,
            transaction_hash,
            output_amount: output,
            shielded: true,
        })
    );
    let (saved, shield) = public_swap_record(wallet, swap);
    assert_eq!(saved.observations().bridge_outcome, outcome);
    assert_eq!(
        shield,
        Some(SwapDestinationOutcome::Shielded {
            block,
            transaction_hash,
        })
    );
}

// A Public account on Ethereum deposits USDC into Across itself, for the handler on Polygon and
// the message that runs its new destination stealth account's shield. The relayer's fill
// shields the whole output to the wallet there, and the wallet verifies it from the fill's
// block.
//
// The record of a delivered swap takes no refund, so the withheld fill is a second deposit of
// the same account: Across reports it expired, refunds it on Ethereum, and the wallet verifies
// the refund to the Public account. Nothing is recovered: the tokens are back in the account.
//
// The account then sells ETH. That deposit takes no approval and pays its amount as the
// transaction's value, which the pool wraps.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_public_deposit_is_filled_and_shielded_on_the_destination_chain() {
    let fork = ForkChain::start().await;
    let destination_fork = ForkChain::start_destination(DESTINATION_CHAIN).await;
    let wallet = Wallet::open(&fork).with_destination(&destination_fork);
    let origin = &wallet.chain;
    let destination = wallet.destination.as_ref().unwrap();
    let owner = &destination.owner;
    let spoke_pool = origin.bridge_origin_profile().unwrap().spoke_pool();
    let profile = destination.chain.bridge_profile().unwrap();
    let (destination_spoke_pool, handler) = (profile.spoke_pool(), profile.multicall_handler());
    let signer = public_signer("deposit");
    let source = signer.address();
    let (sold, minimum) = (
        U256::from(PUBLIC_DEPOSIT),
        U256::from(PUBLIC_DEPOSIT_MINIMUM),
    );
    let terms = PublicTerms {
        sell_token: USDC,
        sell_amount: sold,
        bridged_token: USDC,
        bridged_amount: sold,
        destination_token: DESTINATION_TOKEN,
        destination_minimum: minimum,
        hook_gas_limit: None,
    };
    let allowance = || ForkAllowance::allowanceCall {
        owner: source,
        spender: spoke_pool,
    };
    let approve = async |swap: &PublicSwap| {
        owner
            .submit_public_swap_approvals_with_signer(
                swap.operation,
                swap.id,
                origin,
                &signer,
                PUBLIC_GAS_FEE,
                &mut |_| {},
            )
            .await
    };

    // The account holds gas money and the USDC of two deposits.
    fork.impersonate(source).await;
    fork.add_erc20(USDC, source, sold * U256::from(2)).await;
    fund_relayer(&destination_fork, destination_spoke_pool).await;

    // The first deposit: one approval of the pool, the delivery signed against a quote dated
    // on the fork's clock, then the deposit itself.
    let swap = wallet.public_swap(&destination_fork, &signer, terms).await;
    assert_eq!(swap.plan.approval_gas_limits.len(), 1);
    approve(&swap).await.unwrap();
    assert_eq!(fork.call(USDC, allowance()).await, sold);
    let signed = wallet
        .sign_public_delivery(
            &swap,
            fork.timestamp().await,
            valid_until(PUBLIC_VALID_SECS),
        )
        .await;
    let held = fork.erc20_balance(USDC, source).await;
    let (receipt, deposit) =
        deposit_from_public_account(&wallet, &fork, &signer, &swap, &signed).await;
    assert_eq!(fork.erc20_balance(USDC, source).await, held - sold);
    observe_public_handoff(&wallet, &fork, &swap, &receipt, &deposit).await;

    // The fill pays the handler, which passes the output to the destination account, and that
    // account's shield puts all of it into Railgun.
    let filled = fill_and_shield(
        &destination_fork,
        destination,
        DESTINATION_TOKEN,
        &deposit,
        swap.account,
    )
    .await;
    verify_public_delivery(
        &wallet,
        &destination_fork,
        &swap,
        deposit.outputAmount,
        &filled,
    )
    .await;

    // The second deposit's fill is withheld, and Across reports it expired.
    let withheld = wallet.public_swap(&destination_fork, &signer, terms).await;
    approve(&withheld).await.unwrap();
    let signed = wallet
        .sign_public_delivery(
            &withheld,
            fork.timestamp().await,
            valid_until(PUBLIC_VALID_SECS),
        )
        .await;
    let held = fork.erc20_balance(USDC, source).await;
    let (receipt, deposit) =
        deposit_from_public_account(&wallet, &fork, &signer, &withheld, &signed).await;
    assert_eq!(fork.erc20_balance(USDC, source).await, held - sold);
    observe_public_handoff(&wallet, &fork, &withheld, &receipt, &deposit).await;
    let expired = |refund| {
        across_deposit_record(
            "expired",
            None,
            refund,
            handler,
            DESTINATION_CHAIN,
            deposit.outputAmount,
        )
    };
    let (across, reply, stub) = across_deposit_stub(expired(None)).await;
    assert_eq!(
        owner
            .check_public_swap_bridge(withheld.operation, withheld.id, &across)
            .await
            .unwrap(),
        Some(SwapBridgeOutcome::Refunding)
    );
    let (refunding, shield) = public_swap_record(&wallet, &withheld);
    assert_eq!(shield, Some(SwapDestinationOutcome::Unfilled));
    assert_eq!(refunding.observations().bridge_refund, None);
    let refund =
        || owner.observe_public_swap_refund(withheld.operation, withheld.id, origin, &across);
    // Across names no refund transaction yet.
    assert_eq!(refund().await.unwrap(), None);

    // The SpokePool refunds the deposit's input to the depositor, the Public account.
    fork.impersonate(spoke_pool).await;
    let refunded = fork
        .send(
            TransactionRequest::default()
                .from(spoke_pool)
                .to(USDC)
                .input(
                    ForkTransfer::transferCall {
                        to: source,
                        amount: deposit.inputAmount,
                    }
                    .abi_encode()
                    .into(),
                )
                .gas_limit(200_000),
        )
        .await;
    assert!(refunded.status(), "the SpokePool refunds the deposit");
    *reply.lock().unwrap() = expired(Some(refunded.transaction_hash));
    assert_eq!(
        refund().await.unwrap(),
        None,
        "the refund's block isn't final yet"
    );
    fork.mine(origin.finality_depth).await;
    let verified = Some(SwapObservation {
        block: BlockNumHash::new(refunded.block_number.unwrap(), refunded.block_hash.unwrap()),
        transaction_hash: Some(refunded.transaction_hash),
    });
    assert_eq!(refund().await.unwrap(), verified);
    let (saved, shield) = public_swap_record(&wallet, &withheld);
    assert_eq!(saved.observations().bridge_refund, verified);
    assert_eq!(
        saved.observations().bridge_outcome,
        Some(SwapBridgeOutcome::Refunding)
    );
    assert_eq!(shield, Some(SwapDestinationOutcome::Unfilled));
    stub.abort();
    // The tokens are back in the Public account. The destination account received nothing,
    // and its shield never ran.
    assert_eq!(fork.erc20_balance(USDC, source).await, held);
    let account = withheld.account.executor();
    assert_eq!(
        (
            destination_fork
                .erc20_balance(DESTINATION_TOKEN, account)
                .await,
            execution_nonce(&destination_fork, account).await
        ),
        (U256::ZERO, withheld.account.observed().nonce())
    );

    // ETH is deposited as WETH: no approval, and the amount as the transaction's value.
    let amount = U256::from(SELL_AMOUNT);
    let native = wallet
        .public_swap(
            &destination_fork,
            &signer,
            PublicTerms {
                sell_token: Address::ZERO,
                sell_amount: amount,
                bridged_token: WETH,
                bridged_amount: amount,
                destination_token: POLYGON_WETH,
                destination_minimum: amount - amount / U256::from(100),
                hook_gas_limit: None,
            },
        )
        .await;
    assert!(native.plan.approval_gas_limits.is_empty());
    approve(&native).await.unwrap();
    assert!(
        public_swap_record(&wallet, &native)
            .0
            .transactions()
            .is_empty()
    );
    let signed = wallet
        .sign_public_delivery(
            &native,
            fork.timestamp().await,
            valid_until(PUBLIC_VALID_SECS),
        )
        .await;
    let (ether, wrapped) = (
        fork.native_balance(source).await,
        fork.erc20_balance(WETH, source).await,
    );
    let (receipt, deposit) =
        deposit_from_public_account(&wallet, &fork, &signer, &native, &signed).await;
    let gas = U256::from(receipt.gas_used) * U256::from(receipt.effective_gas_price);
    assert_eq!(
        (
            fork.native_balance(source).await,
            fork.erc20_balance(WETH, source).await
        ),
        (ether - amount - gas, wrapped)
    );
    let sent = public_swap_record(&wallet, &native).0;
    assert!(matches!(
        sent.transactions(),
        [transaction] if transaction.kind == PublicSwapTransactionKind::Deposit
            && transaction.hash == receipt.transaction_hash
            && transaction.transaction.value == Some(amount)
    ));
    observe_public_handoff(&wallet, &fork, &native, &receipt, &deposit).await;
    wallet.finish().await;
}

/// Deploy `SwapMath` on `fork` through the deterministic deployer, as its release does, unless
/// the forked chain already has it.
async fn deploy_swap_math(fork: &ForkChain) {
    if fork.code(SWAP_MATH_ADDRESS).await.is_empty() {
        let receipt = fork
            .send_as_bypass(
                DETERMINISTIC_DEPLOYER,
                swap_math_deployment_calldata(),
                1_000_000,
            )
            .await;
        assert!(
            receipt.status(),
            "the deterministic deployer deploys the math contract"
        );
    }
    assert_eq!(
        alloy::primitives::keccak256(fork.code(SWAP_MATH_ADDRESS).await),
        SWAP_MATH_RUNTIME_CODE_HASH
    );
}

/// Place an order of the Public account `signer` through the wallet: it sells WETH for USDC
/// paid to the account's cow-shed proxy, whose hook batch deposits the proxy's whole balance
/// for a new destination stealth account on `route`'s chain. The account approves the vault
/// relayer, the delivery is signed for the order's buy amount and `validTo` against a quote
/// dated `quote_ahead` seconds ahead of the swap chain's clock, and the order is taken from
/// what the orderbook stub received.
async fn place_public_order(
    wallet: &Wallet,
    fork: &ForkChain,
    destination_fork: &ForkChain,
    signer: &VaultedPublicSigner,
    route: OrderDestination,
    quote_ahead: u64,
) -> PlacedOrder {
    let origin = &wallet.chain;
    let owner = &wallet.destination.as_ref().unwrap().owner;
    let profile = origin.public_swap_profile().unwrap();
    let source = signer.address();
    let proxy = proxy_address(
        profile.cow_shed_factory(),
        profile.cow_shed_implementation(),
        source,
    );
    let sell_amount = U256::from(SELL_AMOUNT);
    // The account holds gas money and the WETH it sells.
    fork.impersonate(source).await;
    fork.add_erc20(WETH, source, sell_amount).await;
    // The limit a review declares for the hook: an account's first batch deploys its proxy.
    let proxy_deployed = !fork.code(proxy).await.is_empty();
    let hook_gas_limit = crate::cow::hook_gas_limit(crate::cow::public_deposit_hook_gas(
        proxy_deployed,
        railgun_wallet::tx::GasEstimateMode::UpperBound,
    ));
    let swap = wallet
        .public_swap(
            destination_fork,
            signer,
            PublicTerms {
                sell_token: WETH,
                sell_amount,
                bridged_token: USDC,
                bridged_amount: U256::from(PUBLIC_ORDER_BUY_AMOUNT),
                destination_token: route.token,
                destination_minimum: U256::from(route.minimum),
                hook_gas_limit: Some(hook_gas_limit),
            },
        )
        .await;
    owner
        .submit_public_swap_approvals_with_signer(
            swap.operation,
            swap.id,
            origin,
            signer,
            PUBLIC_GAS_FEE,
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(
        fork.call(
            WETH,
            ForkAllowance::allowanceCall {
                owner: source,
                spender: profile.vault_relayer(),
            },
        )
        .await,
        sell_amount
    );

    // A new block puts the fork's clock at the present before the quote is dated.
    fork.mine(1).await;
    let quoted = fork.timestamp().await + quote_ahead;
    let valid_to = valid_until(PUBLIC_VALID_SECS);
    let signed = wallet.sign_public_delivery(&swap, quoted, valid_to).await;
    let orderbook = spawn_public_orderbook(origin.chain_id, profile.settlement()).await;
    let outcome = owner
        .submit_public_swap_order_with_signer(
            swap.operation,
            swap.id,
            origin,
            signer,
            &orderbook.client,
            &signed.delivery,
            &signed.terms,
            valid_to,
            new_public_swap_batch_nonce().unwrap(),
            None,
            false,
        )
        .await
        .unwrap();
    let PublicSwapOrderOutcome::Submitted { uid } = outcome else {
        panic!("the order is submitted: {outcome:?}");
    };
    let body = orderbook.orders.lock().unwrap().last().cloned().unwrap();
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    assert!(hooks.pre.is_empty());
    let [hook] = hooks.post.as_slice() else {
        panic!("the order has one post-hook");
    };
    assert_eq!(
        (hook.target, hook.gas_limit),
        (profile.cow_shed_factory(), hook_gas_limit)
    );
    let order = submitted_order(&body);
    assert_eq!(
        (order.sellToken, order.buyToken, order.receiver),
        (WETH, USDC, proxy)
    );
    assert_eq!(
        (order.sellAmount, order.buyAmount, order.validTo),
        (sell_amount, U256::from(PUBLIC_ORDER_BUY_AMOUNT), valid_to)
    );
    PlacedOrder {
        hook: hook.clone(),
        signature: body["signature"].as_str().unwrap().parse().unwrap(),
        swap,
        signed,
        orderbook,
        order,
        uid,
        proxy,
        valid_to,
    }
}

/// Read what became of `placed`'s order through the wallet once the block of `settlement` is
/// final, with the orderbook stub reporting the order's trade there. The wallet must record
/// the trade in that transaction, for `payout`. Returns what its record then holds of the swap.
async fn observe_public_settlement(
    wallet: &Wallet,
    fork: &ForkChain,
    placed: &PlacedOrder,
    settlement: &TransactionReceipt,
    payout: U256,
) -> PublicSwapObservations {
    let origin = &wallet.chain;
    let owner = &wallet.destination.as_ref().unwrap().owner;
    *placed.orderbook.trade_block.lock().unwrap() = settlement.block_number;
    fork.mine(origin.finality_depth).await;
    let record = owner
        .observe_public_swap_order(
            placed.swap.operation,
            placed.swap.id,
            origin,
            &placed.orderbook.client,
        )
        .await
        .unwrap();
    let (_, saved) = record.public_swap_use(placed.swap.id).unwrap();
    let observed = saved.observations();
    let traded = observed
        .traded
        .expect("the final settlement holds the order's trade");
    assert_eq!(
        (traded.block.number, traded.transaction_hash),
        (
            settlement.block_number.unwrap(),
            Some(settlement.transaction_hash)
        )
    );
    assert_eq!(
        observed
            .trade_amounts
            .map(|amounts| (amounts.sell_amount, amounts.buy_amount)),
        Some((placed.order.sellAmount, payout))
    );
    observed
}

/// Place an order of the Public account `signer` whose hook must fail at its settlement, and
/// settle it above its buy amount. The deposit's quote is dated ahead of the fork's clock, so
/// the `SpokePool` refuses it until that time comes. The settlement must succeed and leave its
/// payout at the address of the account's proxy, which the failed hook didn't deploy, and the
/// wallet must record the proxy as holding it while the batch can still run. Returns the order
/// and the payout.
async fn held_public_order(
    wallet: &Wallet,
    fork: &ForkChain,
    destination_fork: &ForkChain,
    signer: &VaultedPublicSigner,
    route: OrderDestination,
) -> (PlacedOrder, U256) {
    let spoke_pool = wallet.chain.bridge_origin_profile().unwrap().spoke_pool();
    let placed = place_public_order(
        wallet,
        fork,
        destination_fork,
        signer,
        route,
        QUOTE_AHEAD_SECS,
    )
    .await;
    let payout = placed.order.buyAmount + U256::from(SURPLUS);
    let receipt = fork
        .settle_with_surplus(
            &placed.order,
            placed.signature.clone(),
            &[],
            std::slice::from_ref(&placed.hook),
            U256::from(SURPLUS),
        )
        .await;
    assert!(receipt.status(), "the settlement survives its failed hook");
    assert!(
        fork.timestamp().await < u64::from(placed.signed.terms.quote_timestamp),
        "the settlement's block precedes the deposit's quote"
    );
    assert!(deposits(&receipt, spoke_pool).is_empty());
    assert_eq!(fork.erc20_balance(USDC, placed.proxy).await, payout);
    assert!(fork.code(placed.proxy).await.is_empty());

    let observed = observe_public_settlement(wallet, fork, &placed, &receipt, payout).await;
    assert_eq!(
        observed.held_by_proxy,
        Some(PublicSwapProxyHolding {
            observation: observed.traded.unwrap(),
            amount: payout,
        })
    );
    assert_eq!((observed.bridge_handoff, observed.deposited), (None, None));
    assert_eq!(
        public_order_state(wallet, &placed.swap),
        Some(PublicSwapOrderState::HeldByProxy {
            amount: payout,
            batch_live: true,
        })
    );
    (placed, payout)
}

/// A Public account on Ethereum places an order that pays its cow-shed proxy, whose hook batch
/// deposits the proxy's whole balance into Across for `route`'s chain. A settlement above the
/// buy amount deposits all of its payout at the signed scale, and the fill shields the whole
/// output to the wallet on that chain.
///
/// With `route.every_outcome`, the account's next order then runs its batch on the deployed
/// proxy. Other accounts follow: one whose hook fails at the settlement and that withdraws the
/// proceeds its proxy holds, one whose order is invalidated on chain, and one whose failed
/// batch someone runs again before its deadline.
async fn public_order_scenario(route: OrderDestination) {
    let fork = ForkChain::start().await;
    let destination_fork = ForkChain::start_destination_from(route.rpc_env, route.chain_id).await;
    let wallet = Wallet::open(&fork).with_destination_on(&destination_fork, route.chain_id);
    let origin = &wallet.chain;
    let destination = wallet.destination.as_ref().unwrap();
    let owner = &destination.owner;
    let profile = origin.public_swap_profile().unwrap();
    let (factory, settlement) = (profile.cow_shed_factory(), profile.settlement());
    let spoke_pool = origin.bridge_origin_profile().unwrap().spoke_pool();
    let bridge = destination.chain.bridge_profile().unwrap();
    let handler = bridge.multicall_handler();
    let (buy_amount, minimum) = (
        U256::from(PUBLIC_ORDER_BUY_AMOUNT),
        U256::from(route.minimum),
    );
    let payout = buy_amount + U256::from(SURPLUS);
    // The batch scales the deposit's output from the proxy's balance, rounding down.
    let scaled = payout * minimum / buy_amount;
    assert!(scaled > minimum);
    // What the wallet's record holds of `placed`'s order after one more read of its chain.
    let observe = async |placed: &PlacedOrder| {
        owner
            .observe_public_swap_order(
                placed.swap.operation,
                placed.swap.id,
                origin,
                &placed.orderbook.client,
            )
            .await
    };

    deploy_swap_math(&fork).await;
    fund_relayer_with(
        &destination_fork,
        bridge.spoke_pool(),
        route.token,
        minimum * U256::from(10),
    )
    .await;

    let signer = public_signer("order");
    let source = signer.address();
    let placed = place_public_order(&wallet, &fork, &destination_fork, &signer, route, 0).await;
    let proxy = placed.proxy;

    // Anyone can run the published batch before the settlement. Its guard reverts while the
    // proxy holds less than the buy amount, and the factory deploys nothing.
    assert!(fork.code(proxy).await.is_empty());
    let early = fork
        .send_as_bypass(factory, placed.hook.call_data.clone(), BATCH_GAS)
        .await;
    assert!(!early.status(), "an early run of the batch reverts");
    assert!(fork.code(proxy).await.is_empty());

    // The solver pays more than the buy amount, and the hook deposits all of it.
    let receipt = fork
        .settle_with_surplus(
            &placed.order,
            placed.signature.clone(),
            &[],
            std::slice::from_ref(&placed.hook),
            U256::from(SURPLUS),
        )
        .await;
    assert!(
        receipt.status(),
        "the solver settles with the batch as its post-hook"
    );
    let settled_deposits = deposits(&receipt, spoke_pool);
    let [deposit] = settled_deposits.as_slice() else {
        panic!("the hook deposits once within its gas limit");
    };
    assert_eq!(
        (deposit.inputAmount, deposit.outputAmount),
        (payout, scaled)
    );
    assert_eq!(
        (deposit.depositor, deposit.recipient),
        (address_to_bytes32(source), address_to_bytes32(handler))
    );
    assert_eq!(deposit.message, placed.signed.message);
    assert_eq!(
        (
            deposit.inputToken,
            deposit.outputToken,
            deposit.destinationChainId
        ),
        (
            address_to_bytes32(USDC),
            address_to_bytes32(route.token),
            U256::from(route.chain_id)
        )
    );
    assert_eq!(fork.erc20_balance(USDC, proxy).await, U256::ZERO);
    assert!(!fork.code(proxy).await.is_empty());
    let deploying_gas = fork
        .call_gas(receipt.transaction_hash, factory, &placed.hook.call_data)
        .await
        .expect("the settlement's trace holds the hook's call");
    assert!(
        deploying_gas <= placed.hook.gas_limit,
        "the hook used {deploying_gas} gas of its limit of {}",
        placed.hook.gas_limit
    );
    println!(
        "MEASURE public_order_hook_gas chain={} proxy=deployed_by_hook used={deploying_gas} limit={}",
        route.chain_id, placed.hook.gas_limit
    );

    // The wallet reads the trade and the hand-off from the settlement once it is final, with
    // the amounts the deposit's event carries.
    let observed = observe_public_settlement(&wallet, &fork, &placed, &receipt, payout).await;
    assert_eq!(
        observed.bridge_handoff,
        Some(SwapBridgeHandoff {
            observation: observed.traded.unwrap(),
            deposit_id: Some(deposit.depositId),
        })
    );
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: payout,
            output_amount: scaled,
        })
    );
    assert_eq!(observed.held_by_proxy, None);
    assert_eq!(
        public_order_state(&wallet, &placed.swap),
        Some(PublicSwapOrderState::Bridged)
    );

    // The destination account shields the whole deposited output, not only the minimum.
    let filled = fill_and_shield(
        &destination_fork,
        destination,
        route.token,
        deposit,
        placed.swap.account,
    )
    .await;
    verify_public_delivery(&wallet, &destination_fork, &placed.swap, scaled, &filled).await;
    if !route.every_outcome {
        drop(placed);
        wallet.finish().await;
        return;
    }

    // The account's next order runs its batch on the deployed proxy, for a lower reviewed gas
    // limit. Settled at exactly its buy amount, it deposits the signed minimums.
    let next = place_public_order(&wallet, &fork, &destination_fork, &signer, route, 0).await;
    assert!(next.hook.gas_limit < placed.hook.gas_limit);
    let receipt = fork
        .settle(
            &next.order,
            next.signature.clone(),
            &[],
            std::slice::from_ref(&next.hook),
        )
        .await;
    assert!(receipt.status(), "the solver settles the next order");
    let exact = deposits(&receipt, spoke_pool);
    let [exact] = exact.as_slice() else {
        panic!("the next hook deposits once within its gas limit");
    };
    assert_eq!(
        (exact.inputAmount, exact.outputAmount),
        (buy_amount, minimum)
    );
    let deployed_gas = fork
        .call_gas(receipt.transaction_hash, factory, &next.hook.call_data)
        .await
        .expect("the settlement's trace holds the hook's call");
    assert!(
        deployed_gas <= next.hook.gas_limit,
        "the hook used {deployed_gas} gas of its limit of {}",
        next.hook.gas_limit
    );
    println!(
        "MEASURE public_order_hook_gas chain={} proxy=deployed_before used={deployed_gas} limit={}",
        route.chain_id, next.hook.gas_limit
    );
    let observed = observe_public_settlement(&wallet, &fork, &next, &receipt, buy_amount).await;
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: buy_amount,
            output_amount: minimum,
        })
    );
    assert_eq!(
        public_order_state(&wallet, &next.swap),
        Some(PublicSwapOrderState::Bridged)
    );
    drop((placed, next));

    // Another account's hook fails at the settlement, and its proxy was never deployed. The
    // account withdraws the proceeds in one transaction through the factory, which deploys
    // the proxy and runs the transfer.
    let signer = public_signer("withdrawal");
    let source = signer.address();
    let (held, payout) = held_public_order(&wallet, &fork, &destination_fork, &signer, route).await;
    let (operation, id) = (held.swap.operation, held.swap.id);
    let review = owner
        .review_public_swap_withdrawal(
            operation,
            id,
            origin,
            PUBLIC_MAX_FEE_PER_GAS,
            PUBLIC_MAX_PRIORITY_FEE_PER_GAS,
        )
        .await
        .unwrap();
    assert_eq!(
        (review.proxy, review.token, review.amount),
        (held.proxy, USDC, payout)
    );
    assert!(!review.proxy_deployed);
    let before = fork.erc20_balance(USDC, source).await;
    let outcome = owner
        .submit_public_swap_withdrawal_with_signer(
            operation,
            id,
            origin,
            &signer,
            PUBLIC_GAS_FEE,
            &review,
            false,
            &mut |_| {},
        )
        .await
        .unwrap();
    let PublicSwapTransactionOutcome::Included {
        transaction_hash, ..
    } = outcome
    else {
        panic!("the withdrawal succeeds: {outcome:?}");
    };
    let withdrawal = fork.receipt(transaction_hash).await;
    assert_eq!(withdrawal.to, Some(factory));
    assert_eq!(
        (
            fork.erc20_balance(USDC, source).await,
            fork.erc20_balance(USDC, held.proxy).await
        ),
        (before + payout, U256::ZERO)
    );
    assert!(!fork.code(held.proxy).await.is_empty());
    assert!(
        withdrawal.gas_used <= review.gas_limit,
        "the withdrawal used {} gas of the reviewed {}",
        withdrawal.gas_used,
        review.gas_limit
    );
    println!(
        "MEASURE public_order_withdrawal_gas proxy=deployed_by_withdrawal used={} limit={}",
        withdrawal.gas_used, review.gas_limit
    );
    // The account sent its approval and this one withdrawal.
    let sent = public_swap_record(&wallet, &held.swap).0;
    assert!(matches!(
        sent.transactions(),
        [approval, withdrawal] if approval.kind == PublicSwapTransactionKind::Approval
            && withdrawal.kind == PublicSwapTransactionKind::Withdrawal
            && withdrawal.hash == transaction_hash
    ));
    // The withdrawal counts once its block is final.
    fork.mine(origin.finality_depth).await;
    observe(&held).await.unwrap();
    let withdrawn = public_swap_record(&wallet, &held.swap).0;
    assert_eq!(
        withdrawn
            .observations()
            .withdrawn
            .and_then(|observation| observation.transaction_hash),
        Some(transaction_hash)
    );
    assert_eq!(
        public_swap_order_state(&withdrawn),
        Some(PublicSwapOrderState::SwappedNotBridged)
    );
    drop(held);

    // Another account invalidates its open order on chain. Its hook batch stays signed, so a
    // second swap of the account that buys the same token is refused until the batch's
    // deadline has passed: a later payout to the proxy can't be taken by the cancelled order's
    // batch.
    let signer = public_signer("cancellation");
    let open = place_public_order(&wallet, &fork, &destination_fork, &signer, route, 0).await;
    let outcome = owner
        .submit_public_swap_invalidation_with_signer(
            open.swap.operation,
            open.swap.id,
            origin,
            &signer,
            PUBLIC_GAS_FEE,
            &open.orderbook.client,
            INVALIDATION_GAS_LIMIT,
            &mut |_| {},
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, PublicSwapTransactionOutcome::Included { .. }),
        "the invalidation succeeds: {outcome:?}"
    );
    let deadline = u64::from(open.valid_to);
    // The sending step saw the invalidation's first inclusion. The settlement state in that
    // block is not final yet, so it cannot conclude cancellation or release admission.
    assert_eq!(
        public_order_state(&wallet, &open.swap),
        Some(PublicSwapOrderState::Open)
    );
    assert_eq!(
        fork.call(
            settlement,
            ForkSwapSettlement::filledAmountCall {
                orderUid: open.uid.0.to_vec().into(),
            },
        )
        .await,
        U256::MAX
    );
    fork.mine(origin.finality_depth).await;
    observe(&open).await.unwrap();
    let cancelled = public_swap_record(&wallet, &open.swap).0;
    assert_eq!(
        public_swap_order_state(&cancelled),
        Some(PublicSwapOrderState::Cancelled {
            retry_at: deadline + 1
        })
    );
    assert_eq!(
        cancelled.observations().cancelled.unwrap().transaction_hash,
        None
    );
    let refused = owner
        .claim_public_swap(public_claim(
            SwapUseId::random().unwrap(),
            ExecutorOperationId::random().unwrap(),
            signer.address(),
            open.swap.terms,
            open.swap.plan.max_gas_cost,
        ))
        .unwrap_err();
    assert!(
        matches!(
            refused.downcast_ref::<ExecutorStoreError>(),
            Some(ExecutorStoreError::PublicSwapBuysSameToken {
                swap,
                chain_id,
                available_at: Some(available_at),
            }) if *swap == open.swap.id
                && *chain_id == route.chain_id
                && *available_at == deadline + 1
        ),
        "the second swap is refused until the batch's deadline: {refused}"
    );
    drop(open);

    // Another account's hook fails at the settlement because its deposit's quote is dated
    // ahead of the fork's clock. Once that time has come, someone runs the same batch again
    // before its deadline, and it deposits what the proxy held.
    let signer = public_signer("late batch");
    let source = signer.address();
    let (late, payout) = held_public_order(&wallet, &fork, &destination_fork, &signer, route).await;
    mine_past(&fork, u64::from(late.signed.terms.quote_timestamp)).await;
    let run = fork
        .send_as_bypass(factory, late.hook.call_data.clone(), BATCH_GAS)
        .await;
    assert!(
        run.status(),
        "the same batch deposits once the pool accepts its quote"
    );
    let deadline = u64::from(late.valid_to);
    assert!(
        fork.timestamp().await <= deadline,
        "the batch ran before its deadline"
    );
    let deposits = deposits(&run, spoke_pool);
    let [deposit] = deposits.as_slice() else {
        panic!("the late run deposits once");
    };
    assert_eq!(
        (deposit.depositor, deposit.inputAmount, deposit.outputAmount),
        (address_to_bytes32(source), payout, scaled)
    );
    assert_eq!(fork.erc20_balance(USDC, late.proxy).await, U256::ZERO);
    assert!(!fork.code(late.proxy).await.is_empty());

    // The batch can run until its deadline, so the wallet looks for no deposit before the
    // first block past it is final. Then it finds this one among the Public account's
    // deposits, and the swap is bridged.
    fork.mine(origin.finality_depth).await;
    observe(&late).await.unwrap();
    assert_eq!(
        public_order_state(&wallet, &late.swap),
        Some(PublicSwapOrderState::HeldByProxy {
            amount: payout,
            batch_live: true,
        })
    );
    mine_past(&fork, deadline).await;
    fork.mine(origin.finality_depth).await;
    observe(&late).await.unwrap();
    let observed = public_swap_record(&wallet, &late.swap).0.observations();
    assert_eq!(
        observed.bridge_handoff,
        Some(SwapBridgeHandoff {
            observation: SwapObservation {
                block: BlockNumHash::new(run.block_number.unwrap(), run.block_hash.unwrap()),
                transaction_hash: Some(run.transaction_hash),
            },
            deposit_id: Some(deposit.depositId),
        })
    );
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: payout,
            output_amount: scaled,
        })
    );
    assert_eq!(
        public_order_state(&wallet, &late.swap),
        Some(PublicSwapOrderState::Bridged)
    );
    let filled = fill_and_shield(
        &destination_fork,
        destination,
        route.token,
        deposit,
        late.swap.account,
    )
    .await;
    verify_public_delivery(&wallet, &destination_fork, &late.swap, scaled, &filled).await;
    drop(late);
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_public_order_deposits_its_whole_payout_and_is_shielded() {
    Box::pin(public_order_scenario(OrderDestination {
        chain_id: DESTINATION_CHAIN,
        rpc_env: DESTINATION_FORK_RPC_URL_ENV,
        token: DESTINATION_TOKEN,
        minimum: 14_900_000,
        every_outcome: true,
    }))
    .await;
}

// BNB Chain's USDC has 18 decimals, so the approved minimum and the output the batch scales
// from the payout are in units a trillion times smaller than the 6-decimal USDC the order buys.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, BNB_FORK_RPC_URL and anvil"]
async fn swap_fork_public_order_scales_its_output_to_an_18_decimal_token() {
    Box::pin(public_order_scenario(OrderDestination {
        chain_id: BNB_CHAIN,
        rpc_env: BNB_FORK_RPC_URL_ENV,
        token: BNB_USDC,
        minimum: 14_900_000_000_000_000_000,
        every_outcome: false,
    }))
    .await;
}

const BASE: u64 = 8453;
const BASE_USDC: Address = alloy::primitives::address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
/// What the permit order sells, in USDC base units.
const PERMIT_SELL_AMOUNT: u64 = 1_000_000_000;
/// What `CoW`'s stub quotes for that sale, in wei of the chain's wrapped native token.
const PERMIT_QUOTED_BUY: u64 = 300_000_000_000_000_000;
/// What Across's stub previews for the order's buy amount, in wei of Polygon's WETH.
const PERMIT_PREVIEW_OUTPUT: u64 = 250_000_000_000_000_000;
/// Nitro's `ArbGasInfo` precompile, which an order's review on Arbitrum One reads.
const ARB_GAS_INFO: Address =
    alloy::primitives::address!("000000000000000000000000000000000000006C");

/// Where the permit order's Public account pays.
#[derive(Clone, Copy)]
struct PermitOrigin {
    chain_id: u64,
    /// The environment variable that names the RPC the chain is forked from.
    rpc_env: &'static str,
    /// The chain's USDC, which has an EIP-2612 permit.
    usdc: Address,
}

/// A Public account on `route`'s chain that holds USDC and no native balance sells it for the
/// chain's wrapped native token, delivered to Polygon. The review plans a permit and no gas
/// from the account, the approvals step sends nothing, and the order carries the signed permit
/// as its pre-hook. The solver's settlement runs that hook through the trampoline, takes the
/// USDC, and runs the post-hook's deposit, which the wallet reads as the swap's hand-off. The
/// account sends no transaction at any point.
async fn public_permit_order_scenario(route: PermitOrigin) {
    use broadcaster_core::contracts::erc20_permit::IERC20Permit;

    let fork = ForkChain::start_origin(route.rpc_env, route.chain_id).await;
    let destination_fork = ForkChain::start_destination(DESTINATION_CHAIN).await;
    let wallet = Wallet::open_on(&fork, route.chain_id).with_destination(&destination_fork);
    let origin = &wallet.chain;
    let destination = wallet.destination.as_ref().unwrap();
    let owner = &destination.owner;
    let profile = origin.public_swap_profile().unwrap();
    let (factory, vault_relayer) = (profile.cow_shed_factory(), profile.vault_relayer());
    let spoke_pool = origin.bridge_origin_profile().unwrap().spoke_pool();
    let bridge = destination.chain.bridge_profile().unwrap();
    let usdc = route.usdc;
    let weth = crate::amounts::wrapped_native_token_for_chain(route.chain_id).unwrap();
    let sell_amount = U256::from(PERMIT_SELL_AMOUNT);
    // The harness settles at the mainnet addresses, which this chain must share.
    assert_eq!(profile.settlement(), SETTLEMENT);
    assert!(
        !fork.code(HOOKS_TRAMPOLINE).await.is_empty(),
        "chain {} has the hooks trampoline",
        route.chain_id
    );
    // anvil runs no Nitro precompile. This one answers every call with an L1 base fee of
    // 0.1 gwei, which only enters the order's gas estimate.
    if route.chain_id == ARBITRUM_ONE {
        fork.set_code(
            ARB_GAS_INFO,
            alloy::primitives::bytes!("6305f5e1005f5260205ff3"),
        )
        .await;
    }
    deploy_swap_math(&fork).await;

    // The account holds the USDC it sells and nothing else: no native balance, no allowance
    // to the vault relayer, and no transaction sent.
    let signer = public_signer("permit");
    let source = signer.address();
    let proxy = proxy_address(factory, profile.cow_shed_implementation(), source);
    fork.add_erc20(usdc, source, sell_amount).await;
    let allowance = || ForkAllowance::allowanceCall {
        owner: source,
        spender: vault_relayer,
    };
    let permit_nonce = || IERC20Permit::noncesCall { owner: source };
    assert_eq!(fork.erc20_balance(usdc, source).await, sell_amount);
    assert_eq!(fork.native_balance(source).await, U256::ZERO);
    assert_eq!(fork.call(usdc, allowance()).await, U256::ZERO);
    assert_eq!(fork.transaction_count(source).await, 0);
    let nonce = fork.call(usdc, permit_nonce()).await;

    // The review reads the token's permit on the fork. `CoW`'s quote and Across's preview
    // come from local stubs.
    let (quote_url, _, quote_stub) = spawn_bridge_stub(move |_| {
        json!({
            "quote": {
                "sellToken": usdc, "buyToken": weth,
                "sellAmount": sell_amount.to_string(),
                "buyAmount": PERMIT_QUOTED_BUY.to_string(), "validTo": 1,
                "feeAmount": "0", "gasAmount": "150000", "gasPrice": "1000000000",
                "sellTokenPrice": "300000000", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        })
        .to_string()
    })
    .await;
    let quote_client = CowOrderbookClient::new(
        OperationHttpClient::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        ),
        quote_url,
        route.chain_id,
    )
    .unwrap();
    let (destination_spoke_pool, previewed) = (bridge.spoke_pool(), fork.timestamp().await);
    let (preview_url, _, preview_stub) = spawn_bridge_stub(move |_| {
        across_quote_at(
            spoke_pool,
            destination_spoke_pool,
            U256::from(PERMIT_PREVIEW_OUTPUT),
            previewed,
        )
    })
    .await;
    let registry = crate::settings::build_effective_token_registry(
        &crate::settings::WalletSettings::default(),
    )
    .unwrap();
    let target = crate::bridge::PublicBridgeDestination {
        destination: crate::bridge::BridgeDestination {
            destination_token: POLYGON_WETH,
            intermediate: weth,
            symbol: "WETH".into(),
            same_asset: true,
            near: None,
        },
        path: crate::bridge::PublicBridgePath::Order,
    };
    let review = owner
        .review_public_swap(crate::PublicSwapReviewRequest {
            origin,
            source,
            sell: crate::bridge::PublicSellAsset::Erc20(usdc),
            sell_amount,
            destination: &target,
            slippage_bps: 50,
            gas_share_bps: 5_000,
            on_shield_failure: BridgeShieldFailure::RefundOnOrigin,
            orderbook: Some(&quote_client),
            across: &across_client(preview_url),
            anchor_cache: None,
            token_registry: &registry,
            max_fee_per_gas: PUBLIC_MAX_FEE_PER_GAS,
            max_priority_fee_per_gas: PUBLIC_MAX_PRIORITY_FEE_PER_GAS,
        })
        .await
        .unwrap();
    quote_stub.abort();
    preview_stub.abort();
    assert!(
        review.signs_permit(),
        "the review plans a permit for USDC's short allowance"
    );
    let plan = review.gas_plan().clone();
    assert!(plan.approval_gas_limits.is_empty());
    assert_eq!(
        (plan.deposit_gas_limit, plan.max_gas_cost),
        (None, U256::ZERO)
    );

    // The swap is claimed with the review's own approval, which prices the permit pre-hook.
    // No anchor checked the stub's price, so the approval acknowledges it.
    let approval = review
        .approval(
            SwapApprovedAccount {
                address: None,
                setup: true,
            },
            Some(U256::from(1_000)),
            true,
        )
        .unwrap();
    let permit_gas_limit = approval.bounds.pre_hook_gas_limit;
    assert_eq!(
        permit_gas_limit,
        crate::cow::hook_gas_limit(crate::cow::PUBLIC_PERMIT_HOOK_GAS)
    );
    let terms = PublicTerms {
        sell_token: usdc,
        sell_amount,
        bridged_token: weth,
        bridged_amount: review.buy_amount().unwrap(),
        destination_token: POLYGON_WETH,
        destination_minimum: review.bridge().destination_minimum,
        hook_gas_limit: review.hook_gas_limit(),
    };
    let (operation, id) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    let claim = PublicSwapUseClaim {
        id,
        origin_chain: route.chain_id,
        source,
        source_scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: TEST_WALLET_ID.to_owned(),
        },
        account: SwapAccountChoice::New(operation),
        destination_token: POLYGON_WETH,
        intent: review.intent(),
        approval,
    };
    let swap = wallet
        .claimed_public_swap(&destination_fork, operation, claim, terms, plan)
        .await;

    // The approvals step has nothing to send.
    let outcome = owner
        .submit_public_swap_approvals_with_signer(
            swap.operation,
            swap.id,
            origin,
            &signer,
            PUBLIC_GAS_FEE,
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome, PublicSwapApprovalsOutcome::Ready);
    assert!(
        public_swap_record(&wallet, &swap)
            .0
            .transactions()
            .is_empty()
    );

    // The order signs the permit under the account's current nonce until the order's
    // `validTo`, proves it on the token, and carries it as its one pre-hook.
    fork.mine(1).await;
    let quoted = fork.timestamp().await;
    let valid_to = valid_until(PUBLIC_VALID_SECS);
    let signed = wallet.sign_public_delivery(&swap, quoted, valid_to).await;
    let orderbook = spawn_public_orderbook(route.chain_id, profile.settlement()).await;
    let outcome = owner
        .submit_public_swap_order_with_signer(
            swap.operation,
            swap.id,
            origin,
            &signer,
            &orderbook.client,
            &signed.delivery,
            &signed.terms,
            valid_to,
            new_public_swap_batch_nonce().unwrap(),
            review.quote_id(),
            false,
        )
        .await
        .unwrap();
    let PublicSwapOrderOutcome::Submitted { uid } = outcome else {
        panic!("the order is submitted: {outcome:?}");
    };
    let body = orderbook.orders.lock().unwrap().last().cloned().unwrap();
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    let ([pre], [post]) = (hooks.pre.as_slice(), hooks.post.as_slice()) else {
        panic!("the order has one pre-hook and one post-hook");
    };
    assert_eq!((pre.target, pre.gas_limit), (usdc, permit_gas_limit));
    let permit = IERC20Permit::permitCall::abi_decode(&pre.call_data).unwrap();
    assert_eq!(
        (permit.owner, permit.spender, permit.value, permit.deadline),
        (source, vault_relayer, sell_amount, U256::from(valid_to))
    );
    assert_eq!(
        (post.target, Some(post.gas_limit)),
        (factory, terms.hook_gas_limit)
    );
    let order = submitted_order(&body);
    assert_eq!(
        (order.sellToken, order.buyToken, order.receiver),
        (usdc, weth, proxy)
    );
    assert_eq!(
        (order.sellAmount, order.buyAmount, order.validTo),
        (sell_amount, terms.bridged_amount, valid_to)
    );
    let recorded = public_swap_record(&wallet, &swap).0;
    assert_eq!(
        recorded
            .order()
            .and_then(|order| order.permit())
            .map(|permit| (permit.nonce(), permit.value(), permit.deadline())),
        Some((nonce, sell_amount, valid_to))
    );
    // Signing and proving the permit changed nothing on chain.
    assert_eq!(fork.call(usdc, permit_nonce()).await, nonce);
    assert_eq!(fork.call(usdc, allowance()).await, U256::ZERO);
    assert_eq!(fork.transaction_count(source).await, 0);
    let placed = PlacedOrder {
        hook: post.clone(),
        signature: body["signature"].as_str().unwrap().parse().unwrap(),
        swap,
        signed,
        orderbook,
        order,
        uid,
        proxy,
        valid_to,
    };

    // The solver settles with both hooks. The settlement takes the USDC only if the permit
    // ran first: nothing else gave the vault relayer an allowance.
    let receipt = fork
        .settle(
            &placed.order,
            placed.signature.clone(),
            &hooks.pre,
            std::slice::from_ref(&placed.hook),
        )
        .await;
    assert!(
        receipt.status(),
        "the solver settles with the permit as its pre-hook and the batch as its post-hook"
    );
    assert_eq!(fork.call(usdc, permit_nonce()).await, nonce + U256::ONE);
    assert_eq!(fork.erc20_balance(usdc, source).await, U256::ZERO);
    assert_eq!(fork.call(usdc, allowance()).await, U256::ZERO);
    let settled = deposits(&receipt, spoke_pool);
    let [deposit] = settled.as_slice() else {
        panic!("the post-hook deposits once within its gas limit");
    };
    assert_eq!(
        (deposit.inputAmount, deposit.outputAmount),
        (terms.bridged_amount, terms.destination_minimum)
    );
    assert_eq!(
        (deposit.depositor, deposit.recipient),
        (
            address_to_bytes32(source),
            address_to_bytes32(bridge.multicall_handler())
        )
    );
    assert_eq!(deposit.message, placed.signed.message);
    assert_eq!(
        (
            deposit.inputToken,
            deposit.outputToken,
            deposit.destinationChainId
        ),
        (
            address_to_bytes32(weth),
            address_to_bytes32(POLYGON_WETH),
            U256::from(DESTINATION_CHAIN)
        )
    );
    // The trampoline's call of the token, from the settlement's trace.
    match fork
        .call_gas(receipt.transaction_hash, usdc, &pre.call_data)
        .await
    {
        Some(used) => {
            assert!(
                used <= pre.gas_limit,
                "the permit used {used} gas of its limit of {}",
                pre.gas_limit
            );
            eprintln!(
                "MEASURE public_permit_hook_gas chain={} used={used} limit={}",
                route.chain_id, pre.gas_limit
            );
        }
        None => eprintln!(
            "MEASURE public_permit_hook_gas chain={} unavailable: no permit call was traced",
            route.chain_id
        ),
    }

    // The wallet reads the trade and the hand-off from the final settlement.
    let observed =
        observe_public_settlement(&wallet, &fork, &placed, &receipt, terms.bridged_amount).await;
    assert_eq!(
        observed.bridge_handoff,
        Some(SwapBridgeHandoff {
            observation: observed.traded.unwrap(),
            deposit_id: Some(deposit.depositId),
        })
    );
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: terms.bridged_amount,
            output_amount: terms.destination_minimum,
        })
    );
    assert_eq!(observed.held_by_proxy, None);
    assert_eq!(
        public_order_state(&wallet, &placed.swap),
        Some(PublicSwapOrderState::Bridged)
    );
    // From the review to the hand-off the account sent nothing and held no native balance.
    assert_eq!(fork.transaction_count(source).await, 0);
    assert_eq!(fork.native_balance(source).await, U256::ZERO);
    drop(placed);
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_public_permit_order_on_ethereum_sends_nothing_from_the_account() {
    Box::pin(public_permit_order_scenario(PermitOrigin {
        chain_id: 1,
        rpc_env: FORK_RPC_URL_ENV,
        usdc: USDC,
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs BASE_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_public_permit_order_on_base_sends_nothing_from_the_account() {
    Box::pin(public_permit_order_scenario(PermitOrigin {
        chain_id: BASE,
        rpc_env: BASE_FORK_RPC_URL_ENV,
        usdc: BASE_USDC,
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs ARBITRUM_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_public_permit_order_on_arbitrum_sends_nothing_from_the_account() {
    Box::pin(public_permit_order_scenario(PermitOrigin {
        chain_id: ARBITRUM_ONE,
        rpc_env: ARBITRUM_FORK_RPC_URL_ENV,
        usdc: ARBITRUM_USDC,
    }))
    .await;
}
