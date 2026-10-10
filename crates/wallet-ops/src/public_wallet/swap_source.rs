use alloy::primitives::{B256, Signature};
use alloy::rpc::types::TransactionRequest;
use eyre::{Result, WrapErr as _, eyre};

use super::submission::{PublicActionHandoff, submit_public_action_step_session};
use super::{
    PublicActionGasFeeSelection, PublicActionProgressStep, PublicActionProgressUpdate,
    PublicAssetId, PublicShieldTransactionProfile, VaultedPublicSigner,
};
use crate::hardware_typed_data::HardwareEip712Model;
use crate::settings::EffectiveChainConfig;
use crate::{HttpContext, TxReceiptOutput, query_rpc_pool_with_http_client};

/// What one transaction of a swap's Public account came to. A reverted transaction is an
/// outcome too: its `receipt.status` is false.
pub(crate) struct PublicSwapStepOutcome {
    pub(crate) receipt: TxReceiptOutput,
    /// The account's nonce after this transaction.
    pub(crate) next_nonce: u64,
    /// The fee the included attempt paid at most, for the swap's next transaction.
    pub(crate) gas_fee: PublicActionGasFeeSelection,
}

/// One transaction a Public account sends for a swap it pays: reuse ordinary preflight, signing
/// and block-scoped receipt observation. The owner supplies a durable handoff and no
/// public-account tracker.
///
/// `transaction.gas` is the reviewed limit, and a higher estimate is an error. Without a
/// `transaction.nonce` the account's current one is read.
pub(crate) async fn submit_public_swap_step(
    step: PublicActionProgressStep,
    transaction: TransactionRequest,
    signer: &VaultedPublicSigner,
    chain: &EffectiveChainConfig,
    gas_fee: PublicActionGasFeeSelection,
    http: &HttpContext,
    handoff: &mut PublicActionHandoff<'_>,
    progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
) -> Result<PublicSwapStepOutcome> {
    let gas_limit = transaction
        .gas
        .ok_or_else(|| eyre!("the swap's transaction has no reviewed gas limit"))?;
    let nonce = transaction.nonce;
    let outcome = submit_public_action_step_session(
        step,
        transaction,
        PublicShieldTransactionProfile::Railoxide,
        PublicShieldTransactionProfile::Railoxide.gas_limit_strategy(PublicAssetId::Native),
        signer,
        "public-swap",
        query_rpc_pool_with_http_client(chain.rpc_route.endpoint_urls(), http),
        chain.finality_depth,
        http,
        None,
        chain.chain_id,
        signer.address(),
        &chain.gas,
        Some(gas_limit),
        nonce,
        gas_fee,
        super::PublicActionStepFeePolicy::Custom,
        None,
        &mut None,
        None,
        Some(handoff),
        progress,
    )
    .await?;
    Ok(PublicSwapStepOutcome {
        receipt: outcome.receipt,
        next_nonce: outcome.next_nonce,
        gas_fee: outcome.gas_fee,
    })
}

/// Sign the EIP-712 payload `typed_data` of a swap with its Public account, as a
/// `WalletConnect` typed-data request is signed: a software account signs it directly, and a
/// hardware account shows clear typed data where its device can and otherwise signs the hash,
/// which needs `hash_fallback_confirmed`. Without that confirmation the error is the
/// `WalletConnectHardwareTypedDataHashFallbackConfirmationRequired` a `WalletConnect` request
/// gets.
///
/// `digest` is what the verifying contract checks. A payload that hashes to anything else is
/// refused before the account or its device sees it, and so is a signature that doesn't
/// recover the account over it.
pub(crate) async fn sign_public_swap_typed_data(
    signer: &VaultedPublicSigner,
    typed_data: serde_json::Value,
    digest: B256,
    hash_fallback_confirmed: bool,
) -> Result<Signature> {
    let typed_data = HardwareEip712Model::from_walletconnect_typed_data_json(typed_data)
        .wrap_err("the swap's typed data")?;
    if typed_data.signing_hash() != digest {
        return Err(eyre!(
            "the swap's typed data differs from what its contract verifies"
        ));
    }
    // The signer reads its device's typed-data mode itself and applies the hash fallback rule.
    let signature = signer
        .sign_typed_data_v4(&typed_data, None, hash_fallback_confirmed)
        .await?;
    if !signature
        .recover_address_from_prehash(&digest)
        .is_ok_and(|recovered| recovered == signer.address())
    {
        return Err(eyre!(
            "the signature isn't the Public account's over the swap's typed data"
        ));
    }
    Ok(signature)
}
