//! Opt-in Ethereum mainnet fork for private swap scenarios.
//!
//! Tests using it are `#[ignore]`d and read `ETH_FORK_RPC_URL`. Tests that also
//! fork the destination chain of a private Bridge delivery read that chain's RPC
//! from `DESTINATION_FORK_RPC_URL`, or from `BNB_FORK_RPC_URL` when that chain is BNB Chain.
//! Tests whose order settles on Base or Arbitrum One read that chain's RPC from
//! `BASE_FORK_RPC_URL` or `ARBITRUM_FORK_RPC_URL`. The `CoW` contracts named here are at the
//! same addresses on all three chains.
//! Set `ANVIL_BIN` when `anvil` is not on `PATH`. Railgun accepts any proof when
//! `tx.origin == 0x…dEaD` (its `VERIFICATION_BYPASS`); verification still runs,
//! so gas matches real proofs. Every transaction here is sent from that address,
//! which is also added as an allow-listed `GPv2` solver, so a settlement runs its
//! hooks through the deployed `HooksTrampoline` with synthetic Railgun proofs.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use alloy::primitives::aliases::U120;
use alloy::primitives::{Address, B256, Bytes, U256, address, keccak256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::cow::{AppDataHook, BUY_NATIVE_TOKEN, Order};
use broadcaster_core::contracts::railgun::{
    BoundParams, CommitmentCiphertext, CommitmentPreimage, G1Point, G2Point, SnarkProof, TokenData,
    Transaction,
};
use url::Url;

pub(crate) const FORK_RPC_URL_ENV: &str = "ETH_FORK_RPC_URL";
pub(crate) const DESTINATION_FORK_RPC_URL_ENV: &str = "DESTINATION_FORK_RPC_URL";
pub(crate) const BNB_FORK_RPC_URL_ENV: &str = "BNB_FORK_RPC_URL";
pub(crate) const BASE_FORK_RPC_URL_ENV: &str = "BASE_FORK_RPC_URL";
pub(crate) const ARBITRUM_FORK_RPC_URL_ENV: &str = "ARBITRUM_FORK_RPC_URL";
/// Railgun's `VERIFICATION_BYPASS` origin, used as sender and solver.
pub(crate) const VERIFICATION_BYPASS: Address =
    address!("000000000000000000000000000000000000dEaD");
pub(crate) const RAILGUN: Address = address!("FA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9");
pub(crate) const SETTLEMENT: Address = address!("9008D19f58AAbD9eD0D60971565AA8510560ab41");
pub(crate) const HOOKS_TRAMPOLINE: Address = address!("60Bf78233f48eC42eE3F101b9a05eC7878728006");
pub(crate) const SOLVER_AUTHENTICATION: Address =
    address!("2c4c28DDBdAc9C5E7055b4C863b72eA0149D8aFE");
pub(crate) const MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");
const SETTLEMENT_GAS: u64 = 12_000_000;

alloy::sol! {
    interface ForkSettlement {
        struct TradeData {
            uint256 sellTokenIndex;
            uint256 buyTokenIndex;
            address receiver;
            uint256 sellAmount;
            uint256 buyAmount;
            uint32 validTo;
            bytes32 appData;
            uint256 feeAmount;
            uint256 flags;
            uint256 executedAmount;
            bytes signature;
        }
        struct InteractionData {
            address target;
            uint256 value;
            bytes callData;
        }
        function settle(address[] tokens, uint256[] clearingPrices, TradeData[] trades, InteractionData[][3] interactions) external;
    }

    interface ForkHooksTrampoline {
        struct Hook {
            address target;
            bytes callData;
            uint256 gasLimit;
        }
        function execute(Hook[] hooks) external;
    }

    interface ForkSolverAuthentication {
        function manager() external view returns (address);
        function addSolver(address solver) external;
    }

    interface ForkRailgun {
        function merkleRoot() external view returns (bytes32);
        function treeNumber() external view returns (uint256);
    }

    interface ForkErc20 {
        function balanceOf(address account) external view returns (uint256);
    }
}

/// The Railgun tree that synthetic transactions prove against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RailgunTree {
    pub(crate) number: u16,
    pub(crate) root: B256,
}

/// A Prague fork of one chain on a free local port, stopped on drop.
pub(crate) struct ForkChain {
    child: Child,
    url: Url,
    provider: DynProvider,
}

impl Drop for ForkChain {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl ForkChain {
    /// Spawn the mainnet fork, impersonate the bypass origin, and allow it as a solver.
    pub(crate) async fn start() -> Self {
        Self::start_origin(FORK_RPC_URL_ENV, 1).await
    }

    /// [`Self::start`] for the chain `chain_id`, forked from the RPC that `url_env` names.
    pub(crate) async fn start_origin(url_env: &str, chain_id: u64) -> Self {
        let fork = Self::spawn(url_env, chain_id).await;
        let manager = fork
            .call(
                SOLVER_AUTHENTICATION,
                ForkSolverAuthentication::managerCall {},
            )
            .await;
        fork.impersonate(manager).await;
        let receipt = fork
            .send(
                TransactionRequest::default()
                    .from(manager)
                    .to(SOLVER_AUTHENTICATION)
                    .input(
                        ForkSolverAuthentication::addSolverCall {
                            solver: VERIFICATION_BYPASS,
                        }
                        .abi_encode()
                        .into(),
                    )
                    .gas_limit(200_000),
            )
            .await;
        assert!(receipt.status(), "the bypass origin becomes a solver");
        fork
    }

    /// Spawn the fork of `chain_id`, the destination chain of a private Bridge delivery, and
    /// impersonate the bypass origin. No order settles there, so it has no solver.
    pub(crate) async fn start_destination(chain_id: u64) -> Self {
        Self::start_destination_from(DESTINATION_FORK_RPC_URL_ENV, chain_id).await
    }

    /// [`Self::start_destination`] from the RPC that `url_env` names.
    pub(crate) async fn start_destination_from(url_env: &str, chain_id: u64) -> Self {
        Self::spawn(url_env, chain_id).await
    }

    /// Spawn a fork of the chain `chain_id` from the RPC that `url_env` names, and impersonate
    /// the bypass origin.
    async fn spawn(url_env: &str, chain_id: u64) -> Self {
        let fork_url = std::env::var(url_env).unwrap_or_else(|_| panic!("{url_env} is not set"));
        let port = loop {
            let port = TcpListener::bind("127.0.0.1:0")
                .and_then(|listener| listener.local_addr())
                .expect("free port")
                .port();
            // Another local service owns this port.
            if port != 8547 {
                break port;
            }
        };
        let child = Command::new(std::env::var("ANVIL_BIN").unwrap_or_else(|_| "anvil".to_owned()))
            .args(["--fork-url", fork_url.as_str(), "--hardfork", "prague"])
            .args(["--port", port.to_string().as_str()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn anvil");
        let url: Url = format!("http://127.0.0.1:{port}").parse().unwrap();
        // Snapshot reverts also rewind account nonces. Read them from the fork for each send.
        let provider = ProviderBuilder::default()
            .with_gas_estimation()
            .with_simple_nonce_management()
            .fetch_chain_id()
            .connect_http(url.clone())
            .erased();
        let fork = Self {
            child,
            url,
            provider,
        };
        let mut ready = false;
        for _ in 0..120 {
            if fork.provider.get_chain_id().await.ok() == Some(chain_id) {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(
            ready,
            "anvil did not start a fork of chain {chain_id} from {url_env}"
        );
        fork.impersonate(VERIFICATION_BYPASS).await;
        fork
    }

    pub(crate) fn url(&self) -> Url {
        self.url.clone()
    }

    async fn raw(&self, method: &'static str, params: impl alloy::rpc::json_rpc::RpcSend) {
        let _: serde_json::Value = self
            .provider
            .raw_request(method.into(), params)
            .await
            .unwrap_or_else(|error| panic!("{method}: {error}"));
    }

    /// Unlock `account` for `eth_sendTransaction` and give it gas money.
    pub(crate) async fn impersonate(&self, account: Address) {
        self.raw("anvil_impersonateAccount", (account,)).await;
        self.raw(
            "anvil_setBalance",
            (account, U256::from(10).pow(U256::from(22))),
        )
        .await;
    }

    /// Increase `holder`'s `token` balance by `amount`.
    pub(crate) async fn add_erc20(&self, token: Address, holder: Address, amount: U256) {
        let balance = self.erc20_balance(token, holder).await;
        self.raw("anvil_dealERC20", (holder, token, balance + amount))
            .await;
    }

    pub(crate) async fn erc20_balance(&self, token: Address, holder: Address) -> U256 {
        self.call(token, ForkErc20::balanceOfCall { account: holder })
            .await
    }

    pub(crate) async fn native_balance(&self, holder: Address) -> U256 {
        self.provider.get_balance(holder).await.unwrap()
    }

    /// How many transactions `holder` has sent.
    pub(crate) async fn transaction_count(&self, holder: Address) -> u64 {
        self.provider.get_transaction_count(holder).await.unwrap()
    }

    pub(crate) async fn mine(&self, blocks: u64) {
        self.raw("anvil_mine", (U256::from(blocks),)).await;
    }

    /// Later blocks are `seconds` further ahead of the local clock.
    pub(crate) async fn increase_time(&self, seconds: u64) {
        self.raw("evm_increaseTime", (U256::from(seconds),)).await;
    }

    /// Snapshot the fork's state, for [`Self::revert`].
    pub(crate) async fn snapshot(&self) -> U256 {
        self.provider
            .raw_request("evm_snapshot".into(), ())
            .await
            .unwrap_or_else(|error| panic!("evm_snapshot: {error}"))
    }

    /// Return to the state at `snapshot`, which anvil then discards.
    pub(crate) async fn revert(&self, snapshot: U256) {
        let reverted: bool = self
            .provider
            .raw_request("evm_revert".into(), (snapshot,))
            .await
            .unwrap_or_else(|error| panic!("evm_revert: {error}"));
        assert!(reverted, "the fork returns to its snapshot");
    }

    /// `account`'s code, empty for an address without any.
    pub(crate) async fn code(&self, account: Address) -> Bytes {
        self.provider.get_code_at(account).await.unwrap()
    }

    /// The receipt of the mined `transaction`.
    pub(crate) async fn receipt(&self, transaction: B256) -> TransactionReceipt {
        self.provider
            .get_transaction_receipt(transaction)
            .await
            .unwrap()
            .expect("transaction receipt")
    }

    /// Replace `account`'s code.
    pub(crate) async fn set_code(&self, account: Address, code: Bytes) {
        self.raw("anvil_setCode", (account, code)).await;
    }

    pub(crate) async fn storage_at(&self, account: Address, slot: U256) -> U256 {
        self.provider.get_storage_at(account, slot).await.unwrap()
    }

    pub(crate) async fn set_storage(&self, account: Address, slot: U256, value: U256) {
        self.raw("anvil_setStorageAt", (account, slot, B256::from(value)))
            .await;
    }

    /// Why `transaction` would revert on the latest state, or `None` if it would succeed.
    pub(crate) async fn revert_reason(&self, transaction: TransactionRequest) -> Option<String> {
        self.provider
            .call(transaction)
            .await
            .err()
            .map(|error| error.to_string())
    }

    /// Gas used by the first call to `to` in `transaction` whose input starts with `input`,
    /// from anvil's call tracer. `None` when the trace is unavailable or has no such call.
    pub(crate) async fn call_gas(
        &self,
        transaction: B256,
        to: Address,
        input: &[u8],
    ) -> Option<u64> {
        fn find(frame: &serde_json::Value, to: &str, input: &str) -> Option<u64> {
            let field = |name: &str| frame[name].as_str().map(str::to_lowercase);
            if field("to").as_deref() == Some(to)
                && field("input").is_some_and(|data| data.starts_with(input))
            {
                let used = field("gasUsed")?;
                return u64::from_str_radix(used.trim_start_matches("0x"), 16).ok();
            }
            frame["calls"]
                .as_array()?
                .iter()
                .find_map(|call| find(call, to, input))
        }
        let trace: serde_json::Value = self
            .provider
            .raw_request(
                "debug_traceTransaction".into(),
                (transaction, serde_json::json!({"tracer": "callTracer"})),
            )
            .await
            .ok()?;
        find(
            &trace,
            &format!("{to:#x}"),
            &Bytes::copy_from_slice(input).to_string(),
        )
    }

    pub(crate) async fn block_number(&self) -> u64 {
        self.provider.get_block_number().await.unwrap()
    }

    /// The latest block's timestamp, the fork's clock.
    pub(crate) async fn timestamp(&self) -> u64 {
        self.provider
            .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
            .await
            .unwrap()
            .expect("latest block")
            .header
            .timestamp
    }

    pub(crate) async fn call<C: SolCall>(&self, to: Address, call: C) -> C::Return {
        let output = self
            .provider
            .call(
                TransactionRequest::default()
                    .to(to)
                    .input(call.abi_encode().into()),
            )
            .await
            .unwrap();
        C::abi_decode_returns(&output).unwrap()
    }

    /// Send from an impersonated account; anvil mines it immediately.
    pub(crate) async fn send(&self, transaction: TransactionRequest) -> TransactionReceipt {
        self.provider
            .send_transaction(transaction)
            .await
            .expect("send transaction")
            .get_receipt()
            .await
            .expect("transaction receipt")
    }

    /// Send from the bypass origin, so Railgun accepts synthetic proofs.
    pub(crate) async fn send_as_bypass(
        &self,
        to: Address,
        input: Bytes,
        gas_limit: u64,
    ) -> TransactionReceipt {
        self.send(
            TransactionRequest::default()
                .from(VERIFICATION_BYPASS)
                .to(to)
                .input(input.into())
                .gas_limit(gas_limit),
        )
        .await
    }

    pub(crate) async fn railgun_tree(&self) -> RailgunTree {
        self.railgun_tree_of(RAILGUN).await
    }

    /// The current tree of the Railgun contract `railgun` on this fork's chain.
    pub(crate) async fn railgun_tree_of(&self, railgun: Address) -> RailgunTree {
        let number = self.call(railgun, ForkRailgun::treeNumberCall {}).await;
        RailgunTree {
            number: u16::try_from(number).unwrap(),
            root: self.call(railgun, ForkRailgun::merkleRootCall {}).await,
        }
    }

    /// Settle one fill-or-kill sell order at exactly its limit price as the bypass
    /// solver. The settlement contract is funded with the buy amount first, in ETH for
    /// `GPv2`'s native buy token, then runs `pre_hooks` and `post_hooks` through the
    /// deployed trampoline, as a solver does with the order's app-data hooks.
    pub(crate) async fn settle(
        &self,
        order: &Order,
        signature: Bytes,
        pre_hooks: &[AppDataHook],
        post_hooks: &[AppDataHook],
    ) -> TransactionReceipt {
        self.settle_with_intra_hooks(order, signature, pre_hooks, &[], post_hooks)
            .await
    }

    /// Like [`Self::settle`], but at a clearing price that pays `surplus` more than the
    /// order's `buyAmount`, as for a solver that found a better price.
    pub(crate) async fn settle_with_surplus(
        &self,
        order: &Order,
        signature: Bytes,
        pre_hooks: &[AppDataHook],
        post_hooks: &[AppDataHook],
        surplus: U256,
    ) -> TransactionReceipt {
        self.settle_paying(
            order,
            signature,
            pre_hooks,
            &[],
            post_hooks,
            order.buyAmount + surplus,
        )
        .await
    }

    /// Like [`Self::settle`], but also runs `intra_hooks` after the settlement records
    /// the trade and pulls in the sell token, and before it pays out the buy token.
    /// A solver controls that ordering.
    pub(crate) async fn settle_with_intra_hooks(
        &self,
        order: &Order,
        signature: Bytes,
        pre_hooks: &[AppDataHook],
        intra_hooks: &[AppDataHook],
        post_hooks: &[AppDataHook],
    ) -> TransactionReceipt {
        self.settle_paying(
            order,
            signature,
            pre_hooks,
            intra_hooks,
            post_hooks,
            order.buyAmount,
        )
        .await
    }

    /// Settle `order` paying `payout` of the buy token, at least its `buyAmount`.
    async fn settle_paying(
        &self,
        order: &Order,
        signature: Bytes,
        pre_hooks: &[AppDataHook],
        intra_hooks: &[AppDataHook],
        post_hooks: &[AppDataHook],
        payout: U256,
    ) -> TransactionReceipt {
        if order.buyToken == BUY_NATIVE_TOKEN {
            let balance = self.native_balance(SETTLEMENT).await;
            self.raw("anvil_setBalance", (SETTLEMENT, balance + payout))
                .await;
        } else {
            self.add_erc20(order.buyToken, SETTLEMENT, payout).await;
        }
        let trampoline = |hooks: &[AppDataHook]| {
            if hooks.is_empty() {
                return Vec::new();
            }
            vec![ForkSettlement::InteractionData {
                target: HOOKS_TRAMPOLINE,
                value: U256::ZERO,
                callData: ForkHooksTrampoline::executeCall {
                    hooks: hooks
                        .iter()
                        .map(|hook| ForkHooksTrampoline::Hook {
                            target: hook.target,
                            callData: hook.call_data.clone(),
                            gasLimit: U256::from(hook.gas_limit),
                        })
                        .collect(),
                }
                .abi_encode()
                .into(),
            }]
        };
        let settle = ForkSettlement::settleCall {
            tokens: vec![order.sellToken, order.buyToken],
            // The sell amount at these prices buys exactly `payout`.
            clearingPrices: vec![payout, order.sellAmount],
            trades: vec![ForkSettlement::TradeData {
                sellTokenIndex: U256::ZERO,
                buyTokenIndex: U256::ONE,
                receiver: order.receiver,
                sellAmount: order.sellAmount,
                buyAmount: order.buyAmount,
                validTo: order.validTo,
                appData: order.appData,
                feeAmount: order.feeAmount,
                // Sell, fill-or-kill, ERC-20 balances, EIP-712 signature.
                flags: U256::ZERO,
                executedAmount: U256::ZERO,
                signature,
            }],
            interactions: [
                trampoline(pre_hooks),
                trampoline(intra_hooks),
                trampoline(post_hooks),
            ],
        };
        self.send_as_bypass(SETTLEMENT, settle.abi_encode().into(), SETTLEMENT_GAS)
            .await
    }
}

/// A field element derived from `label`, below the SNARK scalar field.
pub(crate) fn field_element(label: &[u8]) -> B256 {
    B256::from(U256::from_be_bytes(keccak256(label).0) >> 8)
}

/// Unshield `amount` of `token` to `recipient` as a transaction's last commitment.
pub(crate) fn unshield_to(recipient: Address, token: Address, amount: U256) -> CommitmentPreimage {
    CommitmentPreimage {
        npk: recipient.into_word(),
        token: TokenData::erc20(token),
        value: U120::from(amount),
    }
}

/// A private transaction bound to `executor` that spends `nullifier` against the
/// current root, with one change output and an optional unshield. Its proof is
/// the curve generators, which verification accepts only under the bypass origin.
pub(crate) fn synthetic_transaction(
    tree: RailgunTree,
    nullifier: B256,
    executor: Address,
    unshield: Option<CommitmentPreimage>,
) -> Transaction {
    let decimal = |value: &str| U256::from_str_radix(value, 10).unwrap();
    let generator = G1Point {
        x: U256::ONE,
        y: U256::from(2),
    };
    let mut commitments = vec![field_element(&[nullifier.as_slice(), b"change"].concat())];
    let mut bound = BoundParams::new_transact(
        u32::from(tree.number),
        0,
        1,
        vec![CommitmentCiphertext {
            ciphertext: [0_u8, 1, 2, 3].map(|index| {
                field_element(&[nullifier.as_slice(), b"ciphertext", &[index]].concat())
            }),
            blindedSenderViewingKey: field_element(&[nullifier.as_slice(), b"sender"].concat()),
            blindedReceiverViewingKey: field_element(&[nullifier.as_slice(), b"receiver"].concat()),
            annotationData: Bytes::new(),
            memo: Bytes::new(),
        }],
        executor,
        B256::ZERO,
    );
    let unshield_preimage = if let Some(preimage) = unshield {
        commitments.push(B256::from(preimage.hash()));
        bound.unshield = 1;
        preimage
    } else {
        CommitmentPreimage::empty()
    };
    Transaction {
        proof: SnarkProof {
            a: generator.clone(),
            b: G2Point {
                x: [
                    decimal(
                        "11559732032986387107991004021392285783925812861821192530917403151452391805634",
                    ),
                    decimal(
                        "10857046999023057135944570762232829481370756359578518086990519993285655852781",
                    ),
                ],
                y: [
                    decimal(
                        "4082367875863433681332203403145435568316851327593401208105741076214120093531",
                    ),
                    decimal(
                        "8495653923123431417604973247489272438418190587263600148770280649306958101930",
                    ),
                ],
            },
            c: generator,
        },
        merkleRoot: tree.root,
        nullifiers: vec![nullifier],
        commitments,
        boundParams: bound,
        unshieldPreimage: unshield_preimage,
    }
}
