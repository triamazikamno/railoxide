//! Opt-in fork checks of the Public Shield routes: Mimic Railway native shields from a
//! software wallet's stealth account on each supported chain, the stealth account left
//! holding the value of one that skipped its delegation, the Mimic ERC-20 reference path and
//! the ordinary Railoxide native shield. Each test forks its chain with
//! [`ForkChain`] from the RPC its env var names (`ETH_FORK_RPC_URL`, `BNB_FORK_RPC_URL`,
//! `POLYGON_FORK_RPC_URL`, `ARBITRUM_FORK_RPC_URL`), funds a synthetic Public account only on
//! that fork, and drives the wallet's own preparation and submission against it.

use std::path::PathBuf;
use std::sync::Arc;

use alloy::consensus::{Transaction as _, TxEnvelope};
use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
use alloy::primitives::{Address, B256, Bytes, U256, address, keccak256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionReceipt;
use alloy::signers::local::PrivateKeySigner;
use broadcaster_core::contracts::railgun::Shield;
use local_db::{DbConfig, DbStore};
use zeroize::Zeroizing;

use super::cow_fork::{
    ARBITRUM_FORK_RPC_URL_ENV, BNB_FORK_RPC_URL_ENV, FORK_RPC_URL_ENV, ForkChain, MULTICALL3,
};
use super::helpers::{TEST_MNEMONIC, TEST_PASSWORD, temp_db_root};
use crate::public_wallet::{
    PublicActionGasFeeMode, PublicActionGasFeeSelection, PublicAssetId, PublicShieldRequest,
    PublicShieldTransactionProfile, RailwayNativeShieldPayer, RailwayNativeShieldPreparation,
    RailwayNativeShieldProfile, RailwayNativeShieldUndelivered, ShieldSendOutput, TxReceiptOutput,
    prepare_railway_native_shield, public_shield_recipient, railway_native_shield_profile,
    submit_public_shield,
};
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    DesktopVaultStore, DesktopViewSession, ExecutorOperationId, ExecutorPayloadPurpose,
    ExecutorRecord, KdfParams, WalletSource,
};
use crate::{DesktopPrivateSpendAuthorization, ExecutorOwner, HttpContext, RpcChainRoute};

const POLYGON_FORK_RPC_URL_ENV: &str = "POLYGON_FORK_RPC_URL";
/// 0.01 of the chain's native token.
const NATIVE_AMOUNT: u64 = 10_000_000_000_000_000;
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
/// High enough for any of these forks' base fees; EIP-1559 charges only base fee plus tip.
const MAX_FEE_PER_GAS: u128 = 2_000_000_000_000;
const MAX_PRIORITY_FEE_PER_GAS: u128 = 2_000_000_000;
const GAS_FEE: PublicActionGasFeeSelection = PublicActionGasFeeSelection::Custom {
    max_fee_per_gas: MAX_FEE_PER_GAS,
    max_priority_fee_per_gas: MAX_PRIORITY_FEE_PER_GAS,
};

alloy::sol! {
    interface ForkAllowance {
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

/// A software wallet, with its executor owner, whose imported Public account, from a fixed
/// synthetic key, is funded on `fork`.
struct Payer {
    root: PathBuf,
    store: Arc<DesktopVaultStore>,
    view: Arc<DesktopViewSession>,
    owner: Arc<ExecutorOwner>,
    account: String,
    address: Address,
    chain_id: u64,
    chain: EffectiveChainConfig,
    http: HttpContext,
    provider: DynProvider,
}

impl Payer {
    async fn open(fork: &ForkChain, chain_id: u64) -> Self {
        let root = temp_db_root();
        let db = Arc::new(
            DbStore::open(DbConfig {
                root_dir: root.clone(),
            })
            .expect("open db"),
        );
        let store = Arc::new(DesktopVaultStore::from_db(db));
        store
            .create_vault_with_params(TEST_PASSWORD, KdfParams::new(1024, 1, 1))
            .expect("create vault");
        let wallet_id = "railway-native-fork";
        let metadata = store
            .new_wallet_metadata(TEST_PASSWORD, wallet_id, 0, WalletSource::Imported, "Fork")
            .expect("wallet metadata");
        store
            .import_wallet_mnemonic_with_metadata(
                TEST_PASSWORD,
                wallet_id,
                0,
                "english",
                TEST_MNEMONIC,
                &metadata,
            )
            .expect("import wallet");
        let view = Arc::new(
            store
                .load_view_session(TEST_PASSWORD, wallet_id)
                .expect("view session"),
        );
        let secret = keccak256(b"railoxide railway native shield fork payer");
        let key = PrivateKeySigner::from_bytes(&secret).unwrap();
        let account = store
            .import_public_account(TEST_PASSWORD, &view, &secret.to_string(), None, false)
            .expect("import public account");
        assert_eq!(account.address, key.address());
        let mut chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(chain_id)
        .cloned()
        .unwrap();
        chain.rpc_route = RpcChainRoute::new(chain_id, vec![fork.url()]).with_multicall(MULTICALL3);
        let provider = ProviderBuilder::new().connect_http(fork.url()).erased();
        let http = HttpContext::direct_for_tests();
        let owner = Arc::new(
            ExecutorOwner::new(
                0,
                store.db(),
                Arc::clone(&view),
                chain.clone(),
                http.clone(),
            )
            .expect("executor owner"),
        );
        let payer = Self {
            root,
            store,
            view,
            owner,
            account: account.public_account_uuid,
            address: account.address,
            chain_id,
            chain,
            http,
            provider,
        };
        payer
            .anvil(
                "anvil_setBalance",
                (payer.address, U256::from(10).pow(U256::from(20))),
            )
            .await;
        payer
    }

    async fn anvil(&self, method: &'static str, params: impl alloy::rpc::json_rpc::RpcSend) {
        let _: serde_json::Value = self
            .provider
            .raw_request(method.into(), params)
            .await
            .unwrap_or_else(|error| panic!("{method}: {error}"));
    }

    fn profile(&self) -> RailwayNativeShieldProfile {
        railway_native_shield_profile(
            self.chain_id,
            &self.chain,
            RailwayNativeShieldPayer::Software,
        )
        .unwrap()
    }

    /// The real preparation: the estimate of the complete type-4 request.
    async fn prepare(&self, amount: U256) -> Arc<RailwayNativeShieldPreparation> {
        Arc::new(
            prepare_railway_native_shield(
                self.chain_id,
                &self.chain,
                RailwayNativeShieldPayer::Software,
                self.address,
                public_shield_recipient(&self.view).unwrap(),
                amount,
                &self.http,
            )
            .await
            .expect("prepare the native shield"),
        )
    }

    fn request(
        &self,
        asset: PublicAssetId,
        profile: PublicShieldTransactionProfile,
        amount: U256,
        native_preparation: Option<Arc<RailwayNativeShieldPreparation>>,
    ) -> PublicShieldRequest {
        PublicShieldRequest {
            executor_owner: Some(Arc::clone(&self.owner)),
            transaction_tracking: None,
            chain_id: self.chain_id,
            effective_chain: self.chain.clone(),
            view_session: Arc::clone(&self.view),
            vault_store: Arc::clone(&self.store),
            authorization: Some(authorization()),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: self.account.clone(),
            asset,
            amount,
            profile,
            native_operation: native_preparation
                .is_some()
                .then(|| ExecutorOperationId::random().unwrap()),
            native_preparation,
            gas_fee: GAS_FEE,
            gas_fee_mode: PublicActionGasFeeMode::Custom,
            authorized_fee_ceiling: GAS_FEE,
            command_rx: None,
            event_tx: None,
        }
    }

    async fn shield(
        &self,
        asset: PublicAssetId,
        profile: PublicShieldTransactionProfile,
        amount: U256,
        native_preparation: Option<Arc<RailwayNativeShieldPreparation>>,
    ) -> eyre::Result<ShieldSendOutput> {
        submit_public_shield(
            self.request(asset, profile, amount, native_preparation),
            &self.http,
        )
        .await
    }

    /// The wallet's one stealth account record, which a native shield reserved.
    fn stealth_record(&self) -> ExecutorRecord {
        let records = self.owner.records().unwrap();
        let [record] = &records[..] else {
            panic!("expected one stealth account");
        };
        record.clone()
    }

    async fn transaction(&self, hash: B256) -> TxEnvelope {
        self.provider
            .get_transaction_by_hash(hash)
            .await
            .unwrap()
            .expect("mined transaction")
            .into_inner()
    }

    /// The transaction's limit covers 120% of the gas it used, as floor(estimate * 120 / 100)
    /// does: an estimate is at least the gas used.
    async fn assert_estimate_buffered(&self, receipt: &TxReceiptOutput) {
        let gas_limit = self
            .transaction(receipt.tx_hash.parse().unwrap())
            .await
            .gas_limit();
        assert!(
            u128::from(gas_limit) * 100 + 100 >= u128::from(receipt.gas_used) * 120,
            "limit {gas_limit} for {} gas used",
            receipt.gas_used
        );
    }

    fn finish(self) {
        let root = self.root.clone();
        drop(self);
        let _ = std::fs::remove_dir_all(root);
    }
}

fn authorization() -> DesktopPrivateSpendAuthorization {
    DesktopPrivateSpendAuthorization::from_software_credentials(
        Zeroizing::new(TEST_PASSWORD.to_owned()),
        None,
    )
}

/// The EIP-7702 code of an account delegated to `delegate`.
fn delegation(delegate: Address) -> Bytes {
    [
        EIP7702_DELEGATION_DESIGNATOR.as_slice(),
        delegate.as_slice(),
    ]
    .concat()
    .into()
}

/// The value of `receipt`'s one Railgun `Shield` commitment of `token`, after the protocol fee;
/// value and fee add up to `amount`.
fn shielded(receipt: &TransactionReceipt, railgun: Address, token: Address, amount: U256) -> U256 {
    let commitments = receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.address() == railgun)
        .filter_map(|log| log.log_decode::<Shield>().ok())
        .flat_map(|log| {
            let event = log.inner.data;
            event.commitments.into_iter().zip(event.fees)
        })
        .filter(|(commitment, _)| commitment.token.tokenAddress == token)
        .map(|(commitment, fee)| (U256::from(commitment.value), fee))
        .collect::<Vec<_>>();
    let [(value, fee)] = commitments[..] else {
        panic!("expected one Shield commitment of {token}, found {commitments:?}");
    };
    assert_eq!(value + fee, amount);
    value
}

/// The complete native Mimic route on `chain_id`: real preparation, the payer's type-4
/// transaction delegating the wallet's stealth account and wrapping and shielding its value,
/// and the stealth record holding the payload that was sent.
async fn native_shield_completes(url_env: &str, chain_id: u64) {
    let fork = ForkChain::start_destination_from(url_env, chain_id).await;
    let payer = Payer::open(&fork, chain_id).await;
    let profile = payer.profile();
    let amount = U256::from(NATIVE_AMOUNT);
    let preparation = payer.prepare(amount).await;
    let held = fork
        .erc20_balance(profile.wrapped_native, profile.railgun)
        .await;

    let output = payer
        .shield(
            PublicAssetId::Native,
            PublicShieldTransactionProfile::Railway,
            amount,
            Some(Arc::clone(&preparation)),
        )
        .await
        .expect("the native shield is delivered");
    assert!(output.approve.is_none());
    let hash: B256 = output.shield.tx_hash.parse().unwrap();
    let record = payer.stealth_record();
    let stealth = record.address().unwrap();
    let [payload] = record.issued() else {
        panic!("one issued payload");
    };
    assert_eq!(payload.purpose(), ExecutorPayloadPurpose::Operation);
    assert!(payload.context().inputs().is_empty());
    assert_eq!(payload.transaction_hashes(), [hash]);
    let value = shielded(
        &fork.receipt(hash).await,
        profile.railgun,
        profile.wrapped_native,
        amount,
    );
    assert_eq!(
        fork.erc20_balance(profile.wrapped_native, profile.railgun)
            .await,
        held + value
    );
    assert_eq!(fork.code(stealth).await, delegation(profile.delegate));
    assert_eq!(fork.transaction_count(stealth).await, 1);
    assert_eq!(fork.native_balance(stealth).await, U256::ZERO);

    let TxEnvelope::Eip7702(sent) = payer.transaction(hash).await else {
        panic!("the native shield is a type-4 transaction");
    };
    let sent = sent.tx().clone();
    assert_eq!(sent.to, stealth);
    assert!(sent.gas_limit <= preparation.gas_limit());

    payer.finish();
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn railway_native_shield_fork_completes_on_ethereum() {
    native_shield_completes(FORK_RPC_URL_ENV, 1).await;
}

#[tokio::test]
#[ignore = "needs BNB_FORK_RPC_URL and anvil"]
async fn railway_native_shield_fork_completes_on_bnb_chain() {
    native_shield_completes(BNB_FORK_RPC_URL_ENV, 56).await;
}

#[tokio::test]
#[ignore = "needs POLYGON_FORK_RPC_URL and anvil"]
async fn railway_native_shield_fork_completes_on_polygon() {
    native_shield_completes(POLYGON_FORK_RPC_URL_ENV, 137).await;
}

#[tokio::test]
#[ignore = "needs ARBITRUM_FORK_RPC_URL and anvil"]
async fn railway_native_shield_fork_completes_on_arbitrum() {
    native_shield_completes(ARBITRUM_FORK_RPC_URL_ENV, 42161).await;
}

/// A delegation whose nonce the stealth account has passed is skipped: the payer's
/// transaction succeeds, but only moves the value to the stealth account. The shield fails
/// without its `Shield` event, naming the account, whose record stays for recovery.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn railway_native_shield_fork_leaves_a_skipped_delegation_recoverable() {
    let fork = ForkChain::start_destination_from(FORK_RPC_URL_ENV, 1).await;
    let payer = Payer::open(&fork, 1).await;
    let profile = payer.profile();
    let amount = U256::from(NATIVE_AMOUNT);
    let preparation = payer.prepare(amount).await;
    let held = fork
        .erc20_balance(profile.wrapped_native, profile.railgun)
        .await;
    // Held in the pool, the shield waits while its stealth account passes the signed
    // delegation's nonce 0.
    payer.anvil("evm_setAutomine", (false,)).await;
    let skip = async {
        let pending = async {
            loop {
                if let Ok(records) = payer.owner.records()
                    && let [record] = &records[..]
                    && let Some(sent) = record
                        .issued()
                        .iter()
                        .find_map(|payload| payload.transaction_hashes().first().copied())
                    && let Ok(Some(_)) = payer.provider.get_transaction_by_hash(sent).await
                {
                    break record.address().unwrap();
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        };
        let stealth = tokio::time::timeout(std::time::Duration::from_secs(60), pending)
            .await
            .expect("the native shield reaches the pool");
        payer
            .anvil("anvil_setNonce", (stealth, U256::from(1)))
            .await;
        fork.mine(1).await;
        payer.anvil("evm_setAutomine", (true,)).await;
        stealth
    };
    let (shielded, stealth) = tokio::join!(
        payer.shield(
            PublicAssetId::Native,
            PublicShieldTransactionProfile::Railway,
            amount,
            Some(preparation),
        ),
        skip
    );
    let error = shielded.expect_err("the shield is skipped");
    let undelivered = error
        .downcast_ref::<RailwayNativeShieldUndelivered>()
        .expect("an undelivered shield");
    assert_eq!(undelivered.stealth_account, stealth);
    assert!(fork.code(stealth).await.is_empty());
    assert_eq!(fork.native_balance(stealth).await, amount);
    assert_eq!(
        fork.erc20_balance(profile.wrapped_native, profile.railgun)
            .await,
        held
    );
    assert_eq!(payer.stealth_record().address(), Some(stealth));
    payer.finish();
}

/// The Mimic ERC-20 reference path approves the maximum only when the allowance is short,
/// with 120% estimates, and the ordinary Railoxide native shield still runs through
/// `RelayAdapt`.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn railway_erc20_and_railoxide_native_shields_fork_still_complete() {
    let fork = ForkChain::start_destination_from(FORK_RPC_URL_ENV, 1).await;
    let payer = Payer::open(&fork, 1).await;
    let deployment = payer.chain.railgun.as_ref().unwrap().deployment;
    let railgun = deployment.contract;
    let amount = U256::from(10).pow(U256::from(19));
    fork.add_erc20(DAI, payer.address, amount * U256::from(2))
        .await;

    let first = payer
        .shield(
            PublicAssetId::Erc20(DAI),
            PublicShieldTransactionProfile::Railway,
            amount,
            None,
        )
        .await
        .expect("the first Mimic ERC-20 shield");
    let approve = first
        .approve
        .as_ref()
        .expect("Mimic approves when the allowance is short");
    assert_eq!(
        fork.call(
            DAI,
            ForkAllowance::allowanceCall {
                owner: payer.address,
                spender: railgun,
            },
        )
        .await,
        U256::MAX
    );
    payer.assert_estimate_buffered(approve).await;
    payer.assert_estimate_buffered(&first.shield).await;
    shielded(
        &fork.receipt(first.shield.tx_hash.parse().unwrap()).await,
        railgun,
        DAI,
        amount,
    );

    let second = payer
        .shield(
            PublicAssetId::Erc20(DAI),
            PublicShieldTransactionProfile::Railway,
            amount,
            None,
        )
        .await
        .expect("the second Mimic ERC-20 shield");
    assert!(second.approve.is_none(), "the maximum allowance suffices");
    shielded(
        &fork.receipt(second.shield.tx_hash.parse().unwrap()).await,
        railgun,
        DAI,
        amount,
    );

    let native_amount = U256::from(NATIVE_AMOUNT);
    let wrapped = payer.chain.wrapped_native_token.unwrap();
    let held = fork.erc20_balance(wrapped, railgun).await;
    let native = payer
        .shield(
            PublicAssetId::Native,
            PublicShieldTransactionProfile::Railoxide,
            native_amount,
            None,
        )
        .await
        .expect("the Railoxide native shield");
    let hash: B256 = native.shield.tx_hash.parse().unwrap();
    assert_eq!(
        payer.transaction(hash).await.to(),
        Some(deployment.relay_adapt_contract)
    );
    let value = shielded(&fork.receipt(hash).await, railgun, wrapped, native_amount);
    assert_eq!(fork.erc20_balance(wrapped, railgun).await, held + value);
    payer.finish();
}
