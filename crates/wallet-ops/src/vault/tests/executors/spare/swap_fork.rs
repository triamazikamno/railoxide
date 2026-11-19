//! Opt-in mainnet-fork scenarios for swap outcomes, driven through the wallet's own
//! setup, signing, and observation paths with synthetic Railgun proofs:
//!
//! `ETH_FORK_RPC_URL=<mainnet RPC> cargo test -p wallet-ops swap_fork -- --ignored`
//!
//! Set `ANVIL_BIN` when `anvil` is not on `PATH`.
//!
//! `swap_fork_settlement_on_base_with_both_hooks_completes_at_finality` settles its order on a
//! Base fork instead, from `BASE_FORK_RPC_URL`.
//!
//! The `swap_fork_private_across` scenarios also fork Polygon, the destination chain of their
//! private Bridge delivery, and replay the Across fill there. They read its RPC from
//! `DESTINATION_FORK_RPC_URL`. `swap_fork_private_across_fill_shields_to_the_wallet_on_base`
//! delivers to Base instead, forked from `BASE_FORK_RPC_URL`.
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
    OutputPois, read_json_body, spawn_bridge_stub, spawn_orderbook_on, submitted_order,
};
use super::swap_setup::{
    DESTINATION_CHAIN, DESTINATION_TOKEN, USDC, WETH, broadcaster, password, setup_approval,
    setup_fee_limit,
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

    // Railgun's getter of the nullifiers spent in each tree.
    interface ForkNullifiers {
        function nullifiers(uint256 treeNumber, bytes32 nullifier) external view returns (bool);
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

    // The pinned shared `SpokePool` binding declares neither getter.
    interface ForkSpokePoolBuffers {
        function depositQuoteTimeBuffer() external view returns (uint32);
        function fillDeadlineBuffer() external view returns (uint32);
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
/// How far below its review's suggested minimum a speed-up is approved, in USDC base units.
const SPEED_UP_DISCOUNT: u64 = 100_000;

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

/// The transaction builder the wallet plans swaps on the chain `chain_id` with, which the
/// pre-hook's signature binds. Planning never reads its contracts.
const fn builder(chain_id: u64) -> railgun_wallet::TransactionBuilder {
    railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id,
        railgun_contract: Address::repeat_byte(4),
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

/// An order placed on its executor's setup while that setup was included but not final.
struct IncludedOrder {
    swap: Swap,
    /// The setup as issued, which a reorg can drop and anyone can send again.
    setup: IssuedExecutorTransaction,
    /// The note the setup pays its fee with.
    setup_fee: Utxo,
    /// The fork's state just before the setup was sent.
    before_setup: U256,
}

struct Wallet {
    root: std::path::PathBuf,
    db: Arc<DbStore>,
    vault: DesktopVaultStore,
    view: Arc<DesktopViewSession>,
    chain: crate::settings::EffectiveChainConfig,
    owner: ExecutorOwner,
    positions: AtomicU64,
    /// The buy token and delivery of new swaps. The sell token is the chain's WETH.
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

/// What a signing varies beyond its plan.
#[derive(Clone, Copy)]
struct Signing {
    /// The approved minimum on the destination chain of a Bridge order, which its stubbed
    /// quote also delivers.
    destination_minimum: U256,
    /// The lower continuing minimum accepted with the review.
    continuing_minimum: Option<U256>,
    /// How long the order stays valid, from the local clock.
    valid_for: std::time::Duration,
    /// How far below the review's suggested minimum the order is approved, as the Minimum
    /// field lowers it. The harness prices gas at 1 wei, so a gas share alone moves nothing.
    minimum_discount: U256,
}

impl Signing {
    /// `DESTINATION_MINIMUM` for ten minutes at the review's suggested minimum, with no
    /// continuing minimum accepted.
    fn standard() -> Self {
        Self {
            destination_minimum: U256::from(DESTINATION_MINIMUM),
            continuing_minimum: None,
            valid_for: std::time::Duration::from_mins(10),
            minimum_discount: U256::ZERO,
        }
    }
}

/// The destination chain of private Across orders over its own fork, and the wallet's owner
/// there.
struct Destination {
    chain: crate::settings::EffectiveChainConfig,
    owner: ExecutorOwner,
    /// The token private Across orders deliver there.
    token: Address,
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
        self.with_destination_on(fork, DESTINATION_CHAIN, DESTINATION_TOKEN)
    }

    /// Also open the wallet's owner on the destination chain `chain_id`, over `fork`, where
    /// private Across orders deliver `token`.
    fn with_destination_on(mut self, fork: &ForkChain, chain_id: u64, token: Address) -> Self {
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
        self.destination = Some(Destination {
            chain,
            owner,
            token,
        });
        self
    }

    /// The chain's WETH, which every swap here sells.
    fn weth(&self) -> Address {
        crate::amounts::wrapped_native_token_for_chain(self.chain.chain_id).unwrap()
    }

    fn note(&self, tree: RailgunTree, value: U256) -> Utxo {
        Utxo::new(
            broadcaster_core::notes::Note::new_change(
                self.view.scan_keys().master_public_key,
                self.weth(),
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
        let tree = fork
            .railgun_tree_of(self.chain.require_railgun().unwrap().deployment.contract)
            .await;
        self.swap_at_root(fork, delegated, tree).await
    }

    /// [`Self::swap_from`] with its unshield proven against `tree`'s root.
    async fn swap_at_root(
        &self,
        fork: &ForkChain,
        delegated: DelegatedSwapExecutor,
        tree: RailgunTree,
    ) -> Swap {
        let note = self.note(tree, U256::from(SELL_AMOUNT) * U256::from(2));
        let mut transaction = synthetic_transaction(
            tree,
            self.nullifier(&note),
            delegated.executor(),
            Some(unshield_to(
                delegated.executor(),
                self.weth(),
                U256::from(SELL_AMOUNT),
            )),
        );
        // Railgun rejects bound parameters of another chain.
        transaction.boundParams.chainID = self.chain.chain_id;
        self.submit(fork, delegated, note, Some(vec![transaction]), None)
            .await
    }

    /// Delegate a fresh executor with a real delegation-only setup.
    async fn delegate(&self, fork: &ForkChain) -> DelegatedSwapExecutor {
        let authorization = password();
        let delegate = self.chain.accepted_executor_profile().unwrap().delegate();
        let mut candidate = broadcaster(delegate);
        candidate.chain_id = self.chain.chain_id;
        let operation = ExecutorOperationId::random().unwrap();
        let prepared = self
            .owner
            .prepare_swap_setup(
                operation,
                candidate,
                setup_approval(self.weth(), self.pair.0, self.pair.1.clone()),
                None,
                &authorization,
                crate::OperationHttpClient::direct_for_tests(),
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
        destination_candidate.chain_id = destination.chain.chain_id;
        let mut approval = setup_approval(
            self.weth(),
            USDC,
            SwapDelivery::Bridge(BridgeDelivery {
                provider: BridgeProvider::Across,
                destination_chain: destination.chain.chain_id,
                receiver: Address::ZERO,
                destination_token: destination.token,
                surplus,
                private: Some(BridgePrivateDelivery { on_shield_failure }),
            }),
        );
        approval.bounds.destination_setup_fee = Some(setup_fee_limit(1_000));
        approval.bounds.source_setup_fee = Some(setup_fee_limit(1_000));
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
                client: crate::OperationHttpClient::direct_for_tests(),
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
            .observe_swap_setup(
                destination_operation,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap()
            .status
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
            self.weth(),
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
                client: crate::OperationHttpClient::direct_for_tests(),
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
                crate::OperationHttpClient::direct_for_tests(),
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
                crate::OperationHttpClient::direct_for_tests(),
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
        let (issued, _) = self
            .issue_setup(fork, owner, chain, prepared, authorization)
            .await;
        // The wallet's type-4 setup, delivered as a broadcaster would.
        let receipt = deliver(fork, &issued).await;
        assert!(receipt.status(), "the setup delegates the executor");
        fork.mine(chain.finality_depth).await;
        let SwapSetupStatus::Delegated(delegated) = owner
            .observe_swap_setup(
                prepared.operation(),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap()
            .status
        else {
            panic!("the confirmed setup delegated the executor");
        };
        delegated
    }

    /// Sign `prepared`'s delegation-only setup for `fork`, the fork of `owner`'s `chain`, paying
    /// its fee with a note of its own. Returns the issued setup and that note.
    async fn issue_setup(
        &self,
        fork: &ForkChain,
        owner: &ExecutorOwner,
        chain: &crate::settings::EffectiveChainConfig,
        prepared: &PreparedExecutorOperation,
        authorization: &crate::DesktopPrivateSpendAuthorization,
    ) -> (IssuedExecutorTransaction, Utxo) {
        let executor = prepared.context().executor;
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
            .issue_operation(
                prepared,
                &setup,
                std::slice::from_ref(&fee),
                authorization,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        (issued, fee)
    }

    /// Reserve a fresh executor and send its delegation-only setup, then sign, persist, and
    /// submit its order on the latest read that shows the setup included, before the setup is
    /// final. The pre-hook signs the nonce after the setup's and the post-hook the one after
    /// that, while the record keeps the setup unresolved.
    async fn swap_at_inclusion(&self, fork: &ForkChain) -> IncludedOrder {
        let authorization = password();
        let delegate = self.chain.accepted_executor_profile().unwrap().delegate();
        let operation = ExecutorOperationId::random().unwrap();
        let prepared = self
            .owner
            .prepare_swap_setup(
                operation,
                broadcaster(delegate),
                setup_approval(self.weth(), self.pair.0, self.pair.1.clone()),
                None,
                &authorization,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        let (setup, setup_fee) = self
            .issue_setup(fork, &self.owner, &self.chain, &prepared, &authorization)
            .await;
        let before_setup = fork.snapshot().await;
        // The wallet proves against its Merkle forest, which holds leaves up to the safe head
        // only, so the order's root never includes the setup's block.
        let confirmed_tree = fork.railgun_tree().await;
        let receipt = deliver(fork, &setup).await;
        assert!(receipt.status(), "the setup delegates the executor");
        let included = receipt.block_number.unwrap();

        // The confirmed block doesn't show the setup yet. The latest one does.
        let observation = self
            .owner
            .observe_swap_setup(operation, crate::OperationHttpClient::direct_for_tests())
            .await
            .unwrap();
        assert_eq!(observation.status, SwapSetupStatus::Pending);
        assert_eq!(
            observation
                .inclusion
                .map(|inclusion| (inclusion.block(), inclusion.nonce())),
            Some((included, U256::ONE))
        );
        let confirmed = fork.block_number().await - self.chain.finality_depth;
        let delegated = self
            .owner
            .reuse_swap_account(
                operation,
                confirmed,
                SwapAccountRole::Source,
                SwapAccountUse::Claimed(SwapUseId::first(operation)),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap()
            .delegated()
            .expect("the included setup admits the order");
        assert_eq!(
            (delegated.observed().nonce(), delegated.execution_nonce()),
            (U256::ZERO, U256::ONE)
        );

        let swap = self.swap_at_root(fork, delegated, confirmed_tree).await;
        let record = self.record(operation);
        assert_eq!(
            issued_nonces(&record),
            [
                (ExecutorPayloadPurpose::Operation, U256::ZERO),
                (ExecutorPayloadPurpose::SwapPreHook, U256::ONE),
                (ExecutorPayloadPurpose::SwapPostHook, U256::from(2)),
            ]
        );
        assert_eq!(
            RelayAdapt7702::executeCall::abi_decode(&swap.hooks.pre[0].call_data)
                .unwrap()
                ._nonce,
            U256::ONE
        );
        assert_eq!(
            record.payload_state(setup.payload_hash()),
            Some(ExecutorPayloadState::Pending)
        );
        IncludedOrder {
            swap,
            setup,
            setup_fee,
            before_setup,
        }
    }

    /// Drop `placed`'s setup in a reorg that replaces every block since it was sent with a
    /// longer chain without it. The latest read then shows the setup no longer included.
    async fn drop_setup(&self, fork: &ForkChain, placed: &IncludedOrder) {
        let head = fork.block_number().await;
        fork.revert(placed.before_setup).await;
        fork.mine(head + 1 - fork.block_number().await).await;
        assert!(fork.code(placed.swap.executor).await.is_empty());
        let observation = self
            .owner
            .observe_swap_setup(
                placed.swap.operation,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(
            (observation.status, observation.inclusion),
            (SwapSetupStatus::Pending, None)
        );
    }

    /// Prepare the setup of `operation`'s swap again, at the account's first nonce.
    async fn retry_setup(&self, operation: ExecutorOperationId) -> PreparedExecutorOperation {
        let delegate = self.chain.accepted_executor_profile().unwrap().delegate();
        let retry = self
            .owner
            .resume_swap_setup(
                operation,
                broadcaster(delegate),
                &password(),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(retry.context().execution_nonce, U256::ZERO);
        retry
    }

    /// The executor at its current confirmed nonce, for a retry.
    async fn redelegate(&self, operation: ExecutorOperationId) -> DelegatedSwapExecutor {
        let SwapSetupStatus::Delegated(delegated) = self
            .owner
            .observe_swap_setup(operation, crate::OperationHttpClient::direct_for_tests())
            .await
            .unwrap()
            .status
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
            &builder(self.chain.chain_id),
            &swap_profile,
            delegated,
            notes,
            &SwapAmountRequest {
                sell_token: self.weth(),
                buy_token: self.pair.0,
                amount,
                delivery: self.pair.1.clone(),
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
        self.sign_with(fork, plan, notes, transactions, Signing::standard())
            .await
            .unwrap_or_else(|outcome| panic!("the order is submitted, not {outcome:?}"))
    }

    /// [`Self::sign`] with `terms`, or the outcome of a signing that submitted nothing.
    async fn sign_with(
        &self,
        fork: &ForkChain,
        plan: SwapInputPlan,
        notes: &[Utxo],
        transactions: Option<Vec<Transaction>>,
        terms: Signing,
    ) -> Result<Swap, SwapOrderOutcome> {
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
                "sellToken": self.weth(), "buyToken": buy_token,
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
            terms.valid_for,
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
                destination_minimum: terms.destination_minimum,
                expected_output: terms.destination_minimum,
                expected_output_usd: None,
                fee: Some(U256::ZERO),
                leg: crate::BridgeLegPrice::SameAsset,
                fill_time_sec: None,
                private: private.then_some(crate::SwapPrivateBridgeQuote {
                    quoted_output: terms.destination_minimum + U256::from(DELIVERY_ALLOWANCE),
                    delivery_allowance: U256::from(DELIVERY_ALLOWANCE),
                    destination_shield_fee_bps: crate::RAILGUN_PROTOCOL_FEE_BPS,
                    deposit_floor: None,
                }),
            });
        }
        let (orderbook_url, submissions, orderbook_task) = spawn_orderbook_on(
            self.chain.chain_id,
            self.db.clone(),
            self.view.clone(),
            operation,
            swap_profile.settlement(),
            false,
        )
        .await;
        let orderbook = CowOrderbookClient::new(
            OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
            orderbook_url,
            self.chain.chain_id,
        )
        .unwrap();
        let across = match bridge {
            Some(delivery) => Some(
                self.across_stub(
                    fork,
                    &orderbook,
                    delivery.destination_chain,
                    terms.destination_minimum,
                )
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
                destination: crate::SwapBridgeDestination::Chain {
                    destination: &destination,
                    chain: destination_chain,
                },
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
                private_minimum: review.suggested_private_minimum() - terms.minimum_discount,
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
                destination_minimum: bridge.map(|_| terms.destination_minimum),
                destination: destination_signing,
                continuing_minimum: terms.continuing_minimum,
            })
            .await
            .unwrap();
        orderbook_task.abort();
        if let Some((_, _, across_task)) = across {
            across_task.abort();
        }
        let SwapOrderOutcome::Submitted { uid } = outcome else {
            return Err(outcome);
        };
        let (persisted, body) = submissions.lock().unwrap()[0].clone();
        assert!(persisted);
        let app_data = body["appData"].as_str().unwrap();
        Ok(Swap {
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
        })
    }

    /// Across clients on `orderbook`'s route whose fee quotes of `output` come from a local
    /// stub, and the destination chain `destination_chain`: the wallet's own over its fork, or
    /// else the default one. The quote is dated at the fork's latest block, which the
    /// `SpokePool` checks at the deposit, and its fill deadline is three hours later: past the
    /// order's expiry plus the wallet's margin, and within the `SpokePool`'s buffer. The stub
    /// answers a private order's signing-time request, which carries the handler and its
    /// message, the same way.
    async fn across_stub(
        &self,
        fork: &ForkChain,
        orderbook: &CowOrderbookClient,
        destination_chain: u64,
        output: U256,
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
            across_quote_at(spoke_pool, destination_spoke_pool, output, quoted)
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
        let preview = builder(self.chain.chain_id)
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
                    Some(unshield_to(plan.executor(), self.weth(), note.note.value)),
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
            .observe_swap_settlement(
                swap.operation,
                swap.uid,
                settled,
                crate::OperationHttpClient::direct_for_tests(),
            )
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
                        crate::OperationHttpClient::direct_for_tests(),
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
                        crate::OperationHttpClient::direct_for_tests(),
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
            .issue_operation(
                &preparation,
                &call,
                std::slice::from_ref(&fee),
                &password(),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap()
    }

    /// Speed up `open`, the swap's open order, through the wallet's own planning and signing:
    /// a second order at `open`'s pre-hook nonce that reuses the recorded proof and notes,
    /// invalidates `open` in its pre-hook and is approved `SPEED_UP_DISCOUNT` below its
    /// review's minimum.
    async fn speed_up(&self, fork: &ForkChain, open: &Swap) -> Swap {
        // The account is read again, as before any order. Its nonce is still the shared one.
        let delegated = self.redelegate(open.operation).await;
        let notes = std::slice::from_ref(&open.note);
        let SwapAmountPlan::Fits(plan) = crate::plan_swap_speed_up_inputs(
            &builder(self.chain.chain_id),
            &self.chain.swap_profile().unwrap(),
            &self.record(open.operation),
            delegated,
            notes,
            open.uid,
            std::time::SystemTime::now(),
            None,
        )
        .unwrap() else {
            panic!("the open order's amount fits one order");
        };
        assert_eq!(plan.invalidates(), Some(open.uid));
        self.sign_with(
            fork,
            plan,
            notes,
            None,
            Signing {
                minimum_discount: U256::from(SPEED_UP_DISCOUNT),
                ..Signing::standard()
            },
        )
        .await
        .unwrap_or_else(|outcome| panic!("the speed-up is submitted, not {outcome:?}"))
    }

    /// Sign an order from a fresh executor and speed it up while it is open. Returns the open
    /// order and its speed-up: two orders of one swap use and stealth account whose pre-hooks
    /// share a nonce and spend the same notes, selling the same amount for less.
    async fn overlap(&self, fork: &ForkChain) -> (Swap, Swap) {
        let open = self.swap(fork).await;
        let speed_up = self.speed_up(fork, &open).await;
        assert_eq!(speed_up.operation, open.operation);
        assert_eq!(
            (speed_up.executor, speed_up.swap_use),
            (open.executor, open.swap_use)
        );
        let record = self.record(open.operation);
        let [kept, replacement] = record.swap().unwrap().orders() else {
            panic!("the speed-up is the swap's second order");
        };
        assert_eq!((kept.uid(), replacement.uid()), (open.uid, speed_up.uid));
        assert_eq!(replacement.use_id(), kept.use_id());
        assert_eq!(replacement.invalidates(), Some(open.uid));
        assert_eq!(
            hook_nonces(&record, speed_up.uid),
            hook_nonces(&record, open.uid)
        );
        // Both published pre-hooks sign the shared nonce and spend the same nullifiers, so at
        // most one of them can execute.
        let pre_hook = |swap: &Swap| {
            let call =
                RelayAdapt7702::executeCall::abi_decode(&swap.hooks.pre[0].call_data).unwrap();
            let nullifiers = call
                ._transactions
                .iter()
                .map(|transaction| transaction.nullifiers.clone())
                .collect::<Vec<_>>();
            (call._nonce, nullifiers)
        };
        assert_eq!(pre_hook(&speed_up), pre_hook(&open));
        assert_eq!(speed_up.order.sellAmount, open.order.sellAmount);
        assert!(speed_up.order.buyAmount < open.order.buyAmount);
        (open, speed_up)
    }

    /// Stop the wallet's owner and open another over the same store, as a restart does.
    async fn restart(&mut self) {
        self.owner.shutdown().await;
        self.owner = ExecutorOwner::new(
            1,
            self.db.clone(),
            self.view.clone(),
            self.chain.clone(),
            HttpContext::direct_for_tests(),
        )
        .unwrap();
    }

    async fn observe(
        &self,
        operation: ExecutorOperationId,
        blocks: std::ops::Range<u64>,
    ) -> ExecutorRecord {
        self.owner
            .observe_swap(
                operation,
                blocks,
                crate::OperationHttpClient::direct_for_tests(),
            )
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

/// Run `swap`'s published pre-hook alone, as anyone can before a solver settles its order.
async fn run_pre_hook(fork: &ForkChain, swap: &Swap) -> TransactionReceipt {
    let pre_hook = &swap.hooks.pre[0];
    fork.send_as_bypass(
        swap.executor,
        pre_hook.call_data.clone(),
        pre_hook.gas_limit + 200_000,
    )
    .await
}

/// What `executor` lets the vault relayer of `wallet`'s chain take of the sell token.
async fn relayer_allowance(fork: &ForkChain, wallet: &Wallet, executor: Address) -> U256 {
    fork.call(
        WETH,
        ForkAllowance::allowanceCall {
            owner: executor,
            spender: wallet.chain.swap_profile().unwrap().vault_relayer(),
        },
    )
    .await
}

/// The settlement's filled amount of `uid` on `wallet`'s chain: zero for an unfilled order,
/// the sell amount once it traded, and the maximum once it is invalidated.
async fn order_filled(fork: &ForkChain, wallet: &Wallet, uid: OrderUid) -> U256 {
    fork.call(
        wallet.chain.swap_profile().unwrap().settlement(),
        ForkSwapSettlement::filledAmountCall {
            orderUid: uid.0.to_vec().into(),
        },
    )
    .await
}

/// `record` holds one verified funding, for the swap use and pre-hook nonce `orders` share,
/// found in the transaction `funded`. Both pre-hooks were candidates in that block, so the
/// record names neither as the one that ran. An order whose sibling traded with the funding
/// no longer shares it.
fn assert_shared_funding(record: &ExecutorRecord, orders: [&Swap; 2], funded: B256) {
    let swap = record.swap().unwrap();
    let [funding] = swap.fundings() else {
        panic!("one funding per swap use and pre-hook nonce");
    };
    assert_eq!(
        (
            funding.use_id,
            funding.pre_hook_nonce,
            funding.observation.transaction_hash
        ),
        (orders[0].swap_use, pre_hook_nonce(record), Some(funded))
    );
    for order in orders {
        let recorded = order_of(record, order.uid);
        assert!(recorded.observations().pre_hook_executed.is_none());
        assert_eq!(
            swap.funding(recorded),
            swap.traded_sibling(recorded)
                .is_none()
                .then_some(funding.observation)
        );
    }
}

fn pre_hook_nonce(record: &ExecutorRecord) -> U256 {
    first_order(record).pre_hook().nonce()
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

/// The nonces of the pre-hook and the post-hook of `record`'s order `uid`.
fn hook_nonces(record: &ExecutorRecord, uid: OrderUid) -> (U256, U256) {
    let order = order_of(record, uid);
    (order.pre_hook().nonce(), order.post_hook().unwrap().nonce())
}

/// The account that deposits into the `SpokePool` directly.
const DEPOSITOR: Address = Address::repeat_byte(0xd0);

/// The surplus choices of an Across order, with their labels in measurements.
const SURPLUS_CHOICES: [(BridgeSurplus, &str); 2] = [
    (BridgeSurplus::KeepInAccount, "keep"),
    (BridgeSurplus::Reshield, "reshield"),
];

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

/// The purpose and nonce of each payload that `record`'s account signed, in signing order.
fn issued_nonces(record: &ExecutorRecord) -> Vec<(ExecutorPayloadPurpose, U256)> {
    record
        .issued()
        .iter()
        .map(|payload| (payload.purpose(), payload.nonce()))
        .collect()
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
    bounds.destination_setup_fee = Some(setup_fee_limit(1_000));
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
                crate::OperationHttpClient::direct_for_tests(),
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
            .prepare_public_swap_destination(
                operation,
                id,
                Some(candidate),
                &authorization,
                crate::OperationHttpClient::direct_for_tests(),
            )
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
            .delegated_public_swap_destination(
                operation,
                confirmed,
                id,
                None,
                crate::OperationHttpClient::direct_for_tests(),
            )
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
                client: crate::OperationHttpClient::direct_for_tests(),
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
        .check_public_swap_bridge(
            swap.operation,
            swap.id,
            &across,
            crate::OperationHttpClient::direct_for_tests(),
        )
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

mod across_deposits;
mod private_across;
mod public_deposits;
mod public_orders;
mod recovery;
mod settlements;
mod setup_inclusion;
