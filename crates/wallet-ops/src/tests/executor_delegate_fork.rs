//! Opt-in fork checks that `RelayAdapt7702` delegates listed with one capability are
//! interchangeable for an executor account: a scratch account delegated to the earlier
//! delegate keeps its execution nonce when re-delegated to the newer one, and a payload it
//! signed before the re-delegation still executes after it. Each test forks its chain with
//! [`ForkChain`] from the RPC its env var names (`ETH_FORK_RPC_URL`, `BNB_FORK_RPC_URL`).

use alloy::eips::eip7702::Authorization;
use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
use alloy::primitives::{Address, Bytes, U256, address, keccak256};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::executor::{EXECUTION_NONCE_STORAGE_SLOT, execute_signing_hash};
use broadcaster_core::contracts::railgun::{RelayAdapt7702, RelayAdapt7702ActionData};

use super::cow_fork::{BNB_FORK_RPC_URL_ENV, FORK_RPC_URL_ENV, ForkChain, VERIFICATION_BYPASS};

const GAS_LIMIT: u64 = 500_000;

struct Executor<'a> {
    fork: &'a ForkChain,
    key: PrivateKeySigner,
    chain_id: u64,
}

impl Executor<'_> {
    fn address(&self) -> Address {
        self.key.address()
    }

    /// Signed `execute` calldata with no transactions and no calls, at `nonce`.
    fn execute(&self, nonce: u64) -> Bytes {
        let action = RelayAdapt7702ActionData {
            requireSuccess: true,
            minGasLimit: U256::ZERO,
            calls: vec![],
        };
        let nonce = U256::from(nonce);
        let hash = execute_signing_hash(&[], &action, nonce, self.chain_id, self.address());
        let signature = self.key.sign_hash_sync(&hash).unwrap();
        RelayAdapt7702::executeCall {
            _transactions: vec![],
            _actionData: action,
            _nonce: nonce,
            _signature: Bytes::copy_from_slice(&signature.as_bytes()),
        }
        .abi_encode()
        .into()
    }

    fn transaction(
        &self,
        input: Bytes,
        delegate: Option<Address>,
        account_nonce: u64,
    ) -> TransactionRequest {
        let mut transaction = TransactionRequest::default()
            .from(VERIFICATION_BYPASS)
            .to(self.address())
            .input(input.into())
            .gas_limit(GAS_LIMIT);
        if let Some(delegate) = delegate {
            let authorization = Authorization {
                chain_id: U256::from(self.chain_id),
                address: delegate,
                nonce: account_nonce,
            };
            let signature = self
                .key
                .sign_hash_sync(&authorization.signature_hash())
                .unwrap();
            transaction.authorization_list = Some(vec![authorization.into_signed(signature)]);
        }
        transaction
    }

    /// Send `input` to the account, delegating it to `delegate` first when given.
    async fn send(&self, input: Bytes, delegate: Option<Address>) {
        let account_nonce = self.fork.transaction_count(self.address()).await;
        let receipt = self
            .fork
            .send(self.transaction(input, delegate, account_nonce))
            .await;
        assert!(receipt.status(), "the execution succeeds");
    }

    async fn assert_state(&self, delegate: Address, execution_nonce: u64) {
        let code = self.fork.code(self.address()).await;
        assert_eq!(
            code.as_ref(),
            [
                EIP7702_DELEGATION_DESIGNATOR.as_slice(),
                delegate.as_slice()
            ]
            .concat(),
            "the account is delegated to {delegate}"
        );
        let nonce = U256::from(execution_nonce);
        assert_eq!(
            self.fork
                .call(self.address(), RelayAdapt7702::nonceCall {})
                .await,
            nonce
        );
        assert_eq!(
            self.fork
                .storage_at(self.address(), EXECUTION_NONCE_STORAGE_SLOT)
                .await,
            nonce,
            "the nonce stays in the slot recovery reads"
        );
    }
}

async fn check_interchangeable(url_env: &str, chain_id: u64, earlier: Address, newer: Address) {
    let fork = ForkChain::start_destination_from(url_env, chain_id).await;
    for delegate in [earlier, newer] {
        assert!(
            !fork.code(delegate).await.is_empty(),
            "{delegate} is deployed on chain {chain_id}"
        );
    }
    let executor = Executor {
        fork: &fork,
        key: PrivateKeySigner::from_bytes(&keccak256(b"railoxide executor delegate fork")).unwrap(),
        chain_id,
    };

    executor.send(executor.execute(0), Some(earlier)).await;
    executor.assert_state(earlier, 1).await;

    // Signed while the account runs the earlier delegate's code.
    let presigned = executor.execute(2);

    executor.send(executor.execute(1), Some(newer)).await;
    executor.assert_state(newer, 2).await;

    executor.send(presigned, None).await;
    executor.assert_state(newer, 3).await;

    let replay = executor.transaction(executor.execute(0), None, 0);
    assert!(
        fork.revert_reason(replay).await.is_some(),
        "a payload consumed under the earlier delegate does not replay under the newer one"
    );
}

#[tokio::test]
#[ignore = "requires ETH_FORK_RPC_URL and anvil"]
async fn ethereum_delegates_are_interchangeable() {
    check_interchangeable(
        FORK_RPC_URL_ENV,
        1,
        address!("05aE73C5925d843864AE6f261f3175dE2ebCd963"),
        address!("1F74b4339990Db761bC0168F20D294A3a141497e"),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires BNB_FORK_RPC_URL and anvil"]
async fn bnb_chain_delegates_are_interchangeable() {
    check_interchangeable(
        BNB_FORK_RPC_URL_ENV,
        56,
        address!("48cf4b897f64D81212c1423D78a05E828d0cE19d"),
        address!("a2BFd382c54bb33DF6A13f21acd8770dc1e67D4D"),
    )
    .await;
}
