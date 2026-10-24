//! Stealth-account native shields, in either transaction profile: a type-4 transaction from
//! the payer sends the approved value to a fresh stealth account with its delegation, and the
//! account's signed `RelayAdapt7702` execution atomically wraps and shields it. A payer that
//! can't send it wraps, approves and shields with plain transactions instead, in the
//! Railoxide profile only.
//!
//! Preparation, before review, fixes the raw estimate of the complete type-4 request and the
//! profile's buffered gas, estimated with a throwaway stand-in account and shield key that are
//! never kept or sent.
//! After payer authorization, Public Shield derives the real shield key, and the private
//! wallet's executor owner reserves the stealth account and signs its execution.
//!
//! A revert returns the value to the payer. Only a transaction that skipped the delegation
//! can succeed without the shield, leaving the value at the stealth account, which the
//! Stealth accounts screen recovers.

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy::eips::eip7702::{Authorization, SignedAuthorization};
use alloy::network::TransactionBuilder as _;
use alloy::primitives::aliases::U120;
use alloy::primitives::ruint::UintTryFrom as _;
use alloy::primitives::{Address, Bytes, U256};
use alloy::rpc::types::{Log, TransactionRequest};
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolEvent;
use broadcaster_core::contracts::executor::{execute_signing_hash, native_shield_action_data};
use broadcaster_core::contracts::railgun::{Shield, ShieldRequest, TokenData};
use broadcaster_core::contracts::shield::build_shield_request;
use broadcaster_core::crypto::railgun::AddressData;
use broadcaster_core::deployment::RailgunDeployment;
use eyre::{Result, WrapErr, eyre};
use zeroize::Zeroizing;

use super::gas::{native_shield_gas_limit, public_fee_candidates};
use super::runtime::public_chain_runtime_config;
use super::signer::VaultedPublicSigner;
use super::types::{EvmFeeModel, PublicShieldTransactionProfile};
use crate::desktop::executors::{estimate_delegated_gas, native_shield_execute_calldata};
use crate::rpc_broker::RpcCandidates;
use crate::settings::EffectiveChainConfig;
use crate::{DesktopPrivateSpendAuthorization, HttpContext};

/// Preparations a native Max may make while fitting its amount to the balance.
const MAX_PREPARATION_ATTEMPTS: usize = 3;

static NEXT_PREPARATION_ID: AtomicU64 = AtomicU64::new(1);

/// The contracts a stealth-account native shield uses on one chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeShieldContracts {
    pub chain_id: u64,
    pub railgun: Address,
    pub wrapped_native: Address,
    pub delegate: Address,
}

/// The payer capability the stealth-account native shield needs: a software type-4
/// signature, from a payer the private wallet's approval of its stealth account also admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeShieldPayer {
    /// A software or stealth-account payer of a software private wallet, or a software or
    /// imported payer of a Ledger or Trezor private wallet.
    Software,
    /// A stealth-account payer of a Ledger or Trezor private wallet. It needs a device
    /// approval of its own, which the shield's stealth-account approval doesn't give, as for
    /// a self-broadcast executor unshield's gas payer.
    HardwareWallet,
    /// A hardware-native account, which cannot sign a type-4 transaction.
    Hardware,
}

impl NativeShieldPayer {
    pub(super) const fn of(
        signer: &VaultedPublicSigner,
        authorization: Option<&DesktopPrivateSpendAuthorization>,
    ) -> Self {
        match (signer, authorization) {
            (VaultedPublicSigner::Hardware(_), _) => Self::Hardware,
            // A Ledger or Trezor wallet's software payer is admitted with its vault password.
            (VaultedPublicSigner::Software(_), _)
            | (
                _,
                Some(
                    DesktopPrivateSpendAuthorization::VaultPassword(_)
                    | DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed { .. },
                ),
            ) => Self::Software,
            _ => Self::HardwareWallet,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NativeShieldUnavailable {
    #[error("the stealth-account native shield isn't available on this network")]
    UnsupportedChain,
    #[error(
        "hardware-wallet Public accounts can't sign the EIP-7702 (type-4) transaction of the stealth-account native shield"
    )]
    HardwarePayer,
    #[error(
        "with a Ledger or Trezor wallet, a stealth Public account can't pay for a stealth-account native shield, because one device approval can't cover both it and the shield's stealth account; use a software or imported Public account"
    )]
    HardwareWallet,
    #[error(
        "this network has no base fee, so it can't carry the EIP-7702 (type-4) transaction of the stealth-account native shield"
    )]
    NoBaseFee,
    #[error("this network has no wrapped native token to shield native tokens through")]
    NoWrappedNative,
}

/// The stealth-account native shield's contracts for `chain_id`, or why the route is
/// unavailable. Checked before any signing or funding.
pub fn stealth_native_shield_contracts(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    payer: NativeShieldPayer,
) -> Result<NativeShieldContracts, NativeShieldUnavailable> {
    let pinned =
        RailgunDeployment::for_chain(chain_id).ok_or(NativeShieldUnavailable::UnsupportedChain)?;
    let (Some(private), Some(wrapped_native)) = (
        effective_chain.railgun.as_ref(),
        effective_chain.wrapped_native_token,
    ) else {
        return Err(NativeShieldUnavailable::UnsupportedChain);
    };
    let contracts = NativeShieldContracts {
        chain_id,
        railgun: private.deployment.contract,
        wrapped_native,
        delegate: pinned.relay_adapt_7702_contract,
    };
    match payer {
        NativeShieldPayer::Software => Ok(contracts),
        NativeShieldPayer::HardwareWallet => Err(NativeShieldUnavailable::HardwareWallet),
        NativeShieldPayer::Hardware => Err(NativeShieldUnavailable::HardwarePayer),
    }
}

/// How a native Public Shield reaches the private wallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeShieldRoute {
    /// One prepared type-4 transaction through a fresh stealth account.
    Stealth(NativeShieldContracts),
    /// Plain wrap, approve and shield transactions from the payer, in the Railoxide profile.
    WrapFallback { wrapped_native: Address },
}

/// The stealth route when the chain, its fee model and `payer` allow it, otherwise the
/// Railoxide wrap fallback. Mimic Railway has no fallback, so it reports why the stealth route
/// is unavailable.
pub fn select_native_shield_route(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    payer: NativeShieldPayer,
    profile: PublicShieldTransactionProfile,
    observed_fee_model: Option<EvmFeeModel>,
) -> Result<NativeShieldRoute, NativeShieldUnavailable> {
    let reason = match stealth_native_shield_contracts(chain_id, effective_chain, payer) {
        // A type-4 envelope needs a base fee.
        Ok(_) if observed_fee_model == Some(EvmFeeModel::Legacy) => {
            NativeShieldUnavailable::NoBaseFee
        }
        Ok(contracts) => return Ok(NativeShieldRoute::Stealth(contracts)),
        Err(reason) => reason,
    };
    if profile == PublicShieldTransactionProfile::Railway || effective_chain.railgun.is_none() {
        return Err(reason);
    }
    effective_chain
        .wrapped_native_token
        .map(|wrapped_native| NativeShieldRoute::WrapFallback { wrapped_native })
        .ok_or(NativeShieldUnavailable::NoWrappedNative)
}

/// A prepared stealth-account native shield: the inputs and transaction profile it is bound
/// to and the gas of its complete request.
pub struct NativeShieldPreparation {
    id: u64,
    contracts: NativeShieldContracts,
    profile: PublicShieldTransactionProfile,
    payer: Address,
    recipient: AddressData,
    amount: U256,
    raw_gas_limit: u64,
    gas_limit: u64,
}

impl fmt::Debug for NativeShieldPreparation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeShieldPreparation")
            .field("id", &self.id)
            .field("chain_id", &self.contracts.chain_id)
            .field("profile", &self.profile)
            .field("raw_gas_limit", &self.raw_gas_limit)
            .field("gas_limit", &self.gas_limit)
            .finish_non_exhaustive()
    }
}

impl NativeShieldPreparation {
    /// `gas_limit_buffer` is the chain's, which only the Railoxide profile adds.
    pub(super) fn new(
        contracts: NativeShieldContracts,
        profile: PublicShieldTransactionProfile,
        payer: Address,
        recipient: AddressData,
        amount: U256,
        raw_gas_limit: u64,
        gas_limit_buffer: u64,
    ) -> Self {
        Self {
            id: NEXT_PREPARATION_ID.fetch_add(1, Ordering::Relaxed),
            contracts,
            profile,
            payer,
            recipient,
            amount,
            raw_gas_limit,
            gas_limit: native_shield_gas_limit(profile, raw_gas_limit, gas_limit_buffer),
        }
    }

    /// Unique per preparation, so a stale completion can be told apart.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        self.contracts.chain_id
    }

    /// The transaction profile whose gas buffer and fee policy the preparation follows.
    #[must_use]
    pub const fn profile(&self) -> PublicShieldTransactionProfile {
        self.profile
    }

    #[must_use]
    pub const fn amount(&self) -> U256 {
        self.amount
    }

    #[must_use]
    pub const fn delegate(&self) -> Address {
        self.contracts.delegate
    }

    /// The complete type-4 request's estimate, for the expected cost.
    #[must_use]
    pub const fn raw_gas_limit(&self) -> u64 {
        self.raw_gas_limit
    }

    /// The raw estimate with the profile's buffer: the reviewed signing limit and maximum cost.
    #[must_use]
    pub const fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    /// Rejects a submission whose bound inputs or route differ from this preparation.
    pub(super) fn ensure_matches(
        &self,
        contracts: &NativeShieldContracts,
        profile: PublicShieldTransactionProfile,
        payer: Address,
        recipient: &AddressData,
        amount: U256,
    ) -> Result<()> {
        if *contracts != self.contracts
            || profile != self.profile
            || payer != self.payer
            || amount != self.amount
            || recipient.master_public_key != self.recipient.master_public_key
            || recipient.viewing_public_key != self.recipient.viewing_public_key
        {
            return Err(eyre!(
                "the native shield changed after it was prepared; prepare and review it again"
            ));
        }
        Ok(())
    }

    /// The shield request of the prepared amount under the payer's real shield key.
    pub(super) fn shield_request(&self, shield_private_key: &[u8; 32]) -> Result<ShieldRequest> {
        native_shield_request(
            &self.contracts,
            &self.recipient,
            self.amount,
            shield_private_key,
        )
    }
}

/// A native shield whose transaction succeeded without a proven shield. Its value may be at
/// the stealth account, so a retry must not send to that account again.
#[derive(Debug, thiserror::Error)]
#[error(
    "Native shield transaction {tx_hash} succeeded, but {reason}. Check stealth account {stealth_account} on the Stealth accounts screen and recover its value there."
)]
pub struct NativeShieldUndelivered {
    pub tx_hash: String,
    pub stealth_account: Address,
    reason: &'static str,
}

/// Fails a native shield whose transaction succeeded with `logs` unless they hold a Railgun
/// `Shield` event, which only the shield's own execution can emit in its transaction.
pub(super) fn ensure_native_shield_delivered(
    contracts: &NativeShieldContracts,
    stealth_account: Address,
    tx_hash: &str,
    logs: Result<Vec<Log>>,
) -> Result<(), NativeShieldUndelivered> {
    let undelivered = |reason| NativeShieldUndelivered {
        tx_hash: tx_hash.to_owned(),
        stealth_account,
        reason,
    };
    let logs = logs.map_err(|_| undelivered("its shield couldn't be verified"))?;
    if !logs
        .iter()
        .filter(|log| log.inner.address == contracts.railgun)
        .any(|log| Shield::decode_log_data(&log.inner.data).is_ok())
    {
        return Err(undelivered("it has no shield"));
    }
    Ok(())
}

/// Prepares a stealth-account native shield of `amount` from `payer_address` to `recipient`
/// under `profile`. The recipient needs no private-spend authorization.
pub async fn prepare_native_shield(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    profile: PublicShieldTransactionProfile,
    payer: NativeShieldPayer,
    payer_address: Address,
    recipient: AddressData,
    amount: U256,
    http: &HttpContext,
) -> Result<NativeShieldPreparation> {
    if amount.is_zero() {
        return Err(eyre!("amount is required"));
    }
    effective_chain.require_railgun()?;
    let contracts = stealth_native_shield_contracts(chain_id, effective_chain, payer)?;
    let chain = public_chain_runtime_config(chain_id, effective_chain)?;
    let candidates = public_fee_candidates(
        http,
        &chain
            .rpc_route
            .for_class(crate::RpcTrafficClass::Sensitive)?,
        None,
    )
    .await?;
    // The stealth account and shield key need the authorization collected after review; a
    // random stand-in of each gives the same request shape for the estimate. Neither is kept.
    let stand_in = PrivateKeySigner::random();
    let mut stand_in_key = Zeroizing::new([0_u8; 32]);
    getrandom::fill(stand_in_key.as_mut_slice()).map_err(|_| eyre!("randomness is unavailable"))?;
    let action = native_shield_action_data(
        stand_in.address(),
        native_shield_request(&contracts, &recipient, amount, &stand_in_key)?,
    )
    .wrap_err("build the native shield execution")?;
    // A fresh account has no code, account nonce 0 and execution nonce 0.
    let signature = stand_in
        .sign_hash_sync(&execute_signing_hash(
            &[],
            &action,
            U256::ZERO,
            chain_id,
            stand_in.address(),
        ))
        .wrap_err("sign the native shield execution")?;
    let request = native_shield_transaction_request(
        &contracts,
        payer_address,
        stand_in.address(),
        amount,
        native_shield_execute_calldata(action, U256::ZERO, signature),
        native_shield_authorization(&stand_in, &contracts, 0)?,
    );
    let raw_gas_limit = estimate_native_shield_gas(&candidates, &request).await?;
    Ok(NativeShieldPreparation::new(
        contracts,
        profile,
        payer_address,
        recipient,
        amount,
        raw_gas_limit,
        chain.gas.gas_limit_buffer,
    ))
}

/// Native shield Max: prepares the balance less the prepared maximum gas cost at
/// `max_fee_per_gas`, re-preparing until amount plus that cost fits the balance.
pub async fn prepare_native_shield_max(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    profile: PublicShieldTransactionProfile,
    payer: NativeShieldPayer,
    payer_address: Address,
    recipient: AddressData,
    balance: U256,
    max_fee_per_gas: u128,
    http: &HttpContext,
) -> Result<NativeShieldPreparation> {
    fit_native_shield_max(balance, max_fee_per_gas, |amount| {
        prepare_native_shield(
            chain_id,
            effective_chain,
            profile,
            payer,
            payer_address,
            recipient,
            amount,
            http,
        )
    })
    .await
}

pub(super) async fn fit_native_shield_max<F, Fut>(
    balance: U256,
    max_fee_per_gas: u128,
    mut prepare: F,
) -> Result<NativeShieldPreparation>
where
    F: FnMut(U256) -> Fut,
    Fut: Future<Output = Result<NativeShieldPreparation>>,
{
    let mut amount = balance;
    for _ in 0..MAX_PREPARATION_ATTEMPTS {
        let preparation = prepare(amount).await?;
        let maximum_gas_cost = U256::from(preparation.gas_limit) * U256::from(max_fee_per_gas);
        if amount
            .checked_add(maximum_gas_cost)
            .is_some_and(|exposure| exposure <= balance)
        {
            return Ok(preparation);
        }
        amount = balance
            .checked_sub(maximum_gas_cost)
            .filter(|amount| !amount.is_zero())
            .ok_or_else(|| eyre!("not enough balance after the native shield's maximum gas"))?;
    }
    Err(eyre!(
        "the native shield's gas estimate kept changing; try Max again"
    ))
}

/// Delegates `account`, at its account nonce `nonce`, to the chain's delegate.
pub(super) fn native_shield_authorization(
    account: &PrivateKeySigner,
    contracts: &NativeShieldContracts,
    nonce: u64,
) -> Result<SignedAuthorization> {
    let authorization = Authorization {
        chain_id: U256::from(contracts.chain_id),
        address: contracts.delegate,
        nonce,
    };
    let signature = account
        .sign_hash_sync(&authorization.signature_hash())
        .wrap_err("sign the native shield delegation")?;
    Ok(authorization.into_signed(signature))
}

fn native_shield_request(
    contracts: &NativeShieldContracts,
    recipient: &AddressData,
    amount: U256,
    shield_private_key: &[u8; 32],
) -> Result<ShieldRequest> {
    let value =
        U120::uint_try_from(amount).map_err(|_| eyre!("native shield amount is too large"))?;
    build_shield_request(
        recipient.master_public_key,
        &recipient.viewing_public_key,
        TokenData::erc20(contracts.wrapped_native),
        value,
        shield_private_key,
    )
    .wrap_err("build the native shield request")
}

pub(super) fn native_shield_transaction_request(
    contracts: &NativeShieldContracts,
    payer: Address,
    account: Address,
    amount: U256,
    input: Bytes,
    authorization: SignedAuthorization,
) -> TransactionRequest {
    let mut request = TransactionRequest::default()
        .with_chain_id(contracts.chain_id)
        .with_from(payer)
        .with_to(account)
        .with_value(amount)
        .with_input(input);
    request.authorization_list = Some(vec![authorization]);
    request.transaction_type = Some(4);
    request
}

async fn estimate_native_shield_gas(
    candidates: &RpcCandidates,
    request: &TransactionRequest,
) -> Result<u64> {
    let mut tried = HashSet::new();
    let mut last_error = None;
    while let Some(provider) = candidates.next_untried(&mut tried).await {
        match estimate_delegated_gas(&provider.provider, request).await {
            Ok(estimate) => return Ok(estimate),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.map_or_else(
        || eyre!("no healthy query RPC available"),
        |error| error.wrap_err("estimate the native shield gas"),
    ))
}
