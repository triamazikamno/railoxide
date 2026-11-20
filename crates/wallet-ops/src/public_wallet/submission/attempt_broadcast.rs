#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::public_wallet) async fn submit_public_action_attempt(
    step: PublicActionProgressStep,
    preflight: PublicActionPreflight,
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    signer: &VaultedPublicSigner,
    label: &str,
    event_tx: Option<&PublicActionSessionEventSender>,
    expiry_timestamp: Option<u64>,
    log_transaction_details: bool,
    before_broadcast: &mut (impl FnMut(&SubmittedPublicActionAttempt, &[u8]) -> Result<()> + Send),
) -> Result<SubmittedPublicActionAttempt, PublicActionAttemptError> {
    let mut handed_off_attempt = None;
    sign_send_public_action_transaction(
        query_rpc_pool,
        network_mode,
        signer,
        preflight.tx_req,
        label,
        event_tx,
        expiry_timestamp,
        log_transaction_details,
        &mut |tx_hash, signed| {
            let attempt = SubmittedPublicActionAttempt {
                tx_hash,
                info: PublicActionAttemptInfo {
                    tx_hash: alloy::hex::encode_prefixed(tx_hash),
                    nonce: preflight.nonce,
                    gas_limit: preflight.gas_limit,
                    max_fee_per_gas: preflight.max_fee_per_gas,
                    max_priority_fee_per_gas: preflight.max_priority_fee_per_gas,
                },
                rpc_gas_price: preflight.rpc_gas_price,
                estimated_native_gas_cost: preflight.estimated_native_gas_cost,
                live_native_balance: preflight.live_native_balance,
            };
            before_broadcast(&attempt, signed)?;
            handed_off_attempt = Some(attempt);
            Ok(())
        },
    )
    .await?;
    let attempt = handed_off_attempt.expect("successful broadcast has a registered attempt");
    emit_public_action_event(
        event_tx,
        PublicActionSessionEvent::AttemptSubmitted {
            step,
            attempt: attempt.info.clone(),
        },
    );
    Ok(attempt)
}

async fn sign_send_public_action_transaction(
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    signer: &VaultedPublicSigner,
    tx_req: TransactionRequest,
    label: &str,
    event_tx: Option<&PublicActionSessionEventSender>,
    expiry_timestamp: Option<u64>,
    log_transaction_details: bool,
    before_broadcast: &mut (impl FnMut(FixedBytes<32>, &[u8]) -> Result<()> + Send),
) -> Result<(), PublicActionAttemptError> {
    if log_transaction_details {
        tracing::info!(target: LOG_TARGET,
            from = %tx_req.from.unwrap_or_default(),
            to = ?tx_req.to,
            gas = ?tx_req.gas,
            label,
            "signing and sending public action transaction",
        );
    }
    let signed_tx = signer
        .sign_transaction_request(tx_req, label)
        .await
        .map_err(PublicActionAttemptError::Signing)?;
    emit_refreshed_public_action_hardware_session(event_tx, signer);
    // Stop/abort requested during synchronous hardware approval is observed here before RPC broadcast.
    public_action_before_raw_broadcast_checkpoint().await;
    signer
        .ensure_active()
        .map_err(PublicActionAttemptError::Sending)?;
    broadcast_signed_public_action_transaction(
        query_rpc_pool,
        network_mode,
        signed_tx,
        label,
        expiry_timestamp,
        log_transaction_details,
        before_broadcast,
    )
    .await
}

pub(super) async fn broadcast_signed_public_action_transaction(
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    signed_tx: Vec<u8>,
    label: &str,
    expiry_timestamp: Option<u64>,
    log_transaction_details: bool,
    before_broadcast: &mut (impl FnMut(FixedBytes<32>, &[u8]) -> Result<()> + Send),
) -> Result<(), PublicActionAttemptError> {
    ensure_public_action_broadcast_not_expired(expiry_timestamp, label)
        .map_err(PublicActionAttemptError::Sending)?;
    let tx_hash = keccak256(&signed_tx);
    before_broadcast(tx_hash, &signed_tx).map_err(PublicActionAttemptError::Sending)?;
    let provider_handles = self_broadcast_send_raw_transaction_to_rpc_pool_with_logging(
        query_rpc_pool,
        network_mode,
        signed_tx,
        tx_hash,
        log_transaction_details,
    )
    .await
    .wrap_err_with(|| format!("{label}: send"))
    .map_err(PublicActionAttemptError::Sending)?;
    if log_transaction_details {
        tracing::info!(target: LOG_TARGET,%tx_hash, providers = provider_handles.len(), label, "sent public action transaction");
    }
    Ok(())
}

pub(in crate::public_wallet) fn ensure_public_action_broadcast_not_expired(
    expiry_timestamp: Option<u64>,
    label: &str,
) -> Result<()> {
    let Some(expiry_timestamp) = expiry_timestamp else {
        return Ok(());
    };
    if public_action_current_unix_seconds() >= expiry_timestamp {
        return Err(eyre!(
            "{label}: request expired before transaction broadcast"
        ));
    }
    Ok(())
}

pub(in crate::public_wallet) fn public_action_current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub(in crate::public_wallet) async fn public_action_before_raw_broadcast_checkpoint() {
    tokio::task::yield_now().await;
}
