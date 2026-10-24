use alloy::rpc::types::TransactionRequest;
use eyre::{Result, eyre};

use super::submission::{
    PublicActionHandoff, PublicActionStepEnd, public_action_pools,
    submit_public_action_step_session,
};
use super::{
    PublicActionGasFeeSelection, PublicActionProgressStep, PublicActionProgressUpdate,
    PublicAssetId, PublicShieldTransactionProfile, VaultedPublicSigner,
};
use crate::settings::EffectiveChainConfig;
use crate::{HttpContext, TxReceiptOutput};

/// Reuse ordinary transaction preflight, signing and block-scoped receipt observation.
/// The native owner supplies a durable handoff and deliberately no public-account tracker.
pub(crate) async fn submit_executor_recovery_step(
    step: PublicActionProgressStep,
    transaction: TransactionRequest,
    signer: &VaultedPublicSigner,
    chain: &EffectiveChainConfig,
    gas_fee: PublicActionGasFeeSelection,
    http: &HttpContext,
    handoff: &mut PublicActionHandoff<'_>,
    progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
) -> Result<TxReceiptOutput> {
    let gas_limit = transaction
        .gas
        .ok_or_else(|| eyre!("recovery has no reviewed gas limit"))?;
    let nonce = transaction
        .nonce
        .ok_or_else(|| eyre!("recovery account nonce is unavailable"))?;
    let mut receipt_progress = |mut update: PublicActionProgressUpdate| {
        if update.status == super::PublicActionProgressStatus::Done {
            update.status = super::PublicActionProgressStatus::Pending;
            update.message = Some("Receipt observed; checking recovery effects".to_owned());
        }
        progress(update);
    };
    let pools = public_action_pools(&chain.rpc_route, http)?;
    let outcome = submit_public_action_step_session(
        step,
        transaction,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        PublicShieldTransactionProfile::Railoxide.gas_limit_strategy(),
        signer,
        "executor-recovery",
        &pools,
        chain.finality_depth,
        http,
        None,
        chain.chain_id,
        signer.address(),
        &chain.gas,
        None,
        Some(gas_limit),
        Some(nonce),
        gas_fee,
        super::PublicActionStepFeePolicy::Custom,
        None,
        &mut None,
        None,
        Some(handoff),
        PublicActionStepEnd::Receipt,
        &mut receipt_progress,
    )
    .await?
    .into_receipt()?;
    Ok(outcome.receipt)
}
