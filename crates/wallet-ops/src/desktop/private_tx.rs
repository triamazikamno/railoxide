use super::*;
use eyre::eyre;
use railgun_wallet::TransactionCall;
use railgun_wallet::tx::{GasEstimateMode, RailgunGasModel};

mod estimate;
mod plan;
mod sponsored_quote;
mod sponsored_submit;
mod submit;

pub use estimate::*;
pub use plan::*;
pub use sponsored_quote::*;
pub use sponsored_submit::*;
pub use submit::*;

pub async fn prepare_desktop_unshield_calldata(
    request: DesktopUnshieldCalldataRequest,
    http: &HttpContext,
) -> Result<PreparedUnshieldCall> {
    let prepared = prepare_desktop_unshield_plan_without_broadcaster_fee(
        DesktopUnshieldPlanRequest {
            executor: None,
            chain_id: request.chain_id,
            effective_chain: &request.effective_chain,
            view_session: request.view_session.as_ref(),
            session: request.session.as_ref(),
            vault_store: request.vault_store.as_ref(),
            spend_authorization: &request.spend_authorization,
            token: request.token,
            amount: request.amount,
            fee_mode: request.fee_mode,
            recipient: request.recipient,
            unwrap: request.unwrap,
            native_top_up: request.native_top_up,
            verify_proof: request.verify_proof,
            progress_tx: request.progress_tx.as_ref(),
        },
        http,
    )
    .await?;

    update_transaction_generation_stage(
        request.progress_tx.as_ref(),
        TransactionGenerationStage::GeneratingPoiProofs,
    );
    persist_manual_unshield_pending_pois(
        &prepared.plan,
        request.session.as_ref(),
        request.chain_id,
        request.view_session.wallet_id(),
        &prepared.prover,
        request.verify_proof,
        http,
        "generate manual unshield pending output pre-transaction POI",
    )
    .await?;

    Ok(prepared_unshield_call_from_plan(
        request.chain_id,
        request.token,
        request.amount,
        request.fee_mode,
        request.recipient,
        request.unwrap,
        prepared.max_spendable,
        &prepared.plan,
        prepared.native_top_up,
    ))
}

pub async fn prepare_desktop_send_calldata(
    request: DesktopSendCalldataRequest,
    http: &HttpContext,
) -> Result<PreparedSendCall> {
    let recipient = request.recipient.trim().to_string();
    let prepared = prepare_desktop_send_plan_without_broadcaster_fee(
        DesktopSendPlanRequest {
            chain_id: request.chain_id,
            effective_chain: &request.effective_chain,
            view_session: request.view_session.as_ref(),
            session: request.session.as_ref(),
            vault_store: request.vault_store.as_ref(),
            spend_authorization: &request.spend_authorization,
            token: request.token,
            amount: request.amount,
            recipient: &recipient,
            verify_proof: request.verify_proof,
            progress_tx: request.progress_tx.as_ref(),
        },
        http,
    )
    .await?;

    update_transaction_generation_stage(
        request.progress_tx.as_ref(),
        TransactionGenerationStage::GeneratingPoiProofs,
    );
    persist_manual_send_pending_pois(
        &prepared.plan,
        request.session.as_ref(),
        request.chain_id,
        request.view_session.wallet_id(),
        &prepared.prover,
        request.verify_proof,
        http,
        "generate manual send pending output pre-transaction POI",
    )
    .await?;

    Ok(prepared_send_call_from_plan(
        request.chain_id,
        request.token,
        request.amount,
        recipient,
        prepared.max_spendable,
        &prepared.plan,
    ))
}

#[cfg(test)]
mod tests;
