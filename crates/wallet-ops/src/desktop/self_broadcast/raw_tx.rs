#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::desktop) enum SelfBroadcastRawTxBroadcastOutcome {
    Accepted,
    AlreadyKnown,
    Rejected(String),
}

pub(in crate::desktop) struct SelfBroadcastRawTxBroadcastResult {
    pub(in crate::desktop) provider_handle: ProviderHandle,
    pub(in crate::desktop) outcome: SelfBroadcastRawTxBroadcastOutcome,
}

pub(in crate::desktop) async fn sign_send_self_broadcast_transaction(
    query_rpc_pool: &QueryRpcPool,
    network_mode: WalletNetworkMode,
    signer: &VaultedPublicSigner,
    tx_req: TransactionRequest,
    session: &WalletSession,
    pending_spent_inputs: &[Utxo],
    event_tx: Option<&SelfBroadcastSessionEventSender>,
    before_broadcast: impl FnOnce(FixedBytes<32>) -> Result<()> + Send,
) -> Result<()> {
    let executor = if let Some(owner) = session.executor_owner() {
        owner
            .transaction_identity(&tx_req)?
            .map(|identity| (owner, identity))
    } else {
        None
    };
    tracing::info!(
        target: LOG_TARGET,
        gas = ?tx_req.gas,
        "signing and sending self-broadcast transaction",
    );
    let signed_tx = signer
        .sign_transaction_request(tx_req, "self-broadcast")
        .await?;
    emit_refreshed_self_broadcast_hardware_session(event_tx, signer);
    let tx_hash = keccak256(&signed_tx);
    if let Some((owner, identity)) = &executor {
        owner.record_submission(*identity, tx_hash)?;
    }
    signer.ensure_active()?;
    before_broadcast(tx_hash)?;
    let provider_handles = self_broadcast_send_raw_transaction_to_rpc_pool(
        query_rpc_pool,
        network_mode,
        signed_tx,
        tx_hash,
    )
    .await
    .wrap_err("self-broadcast: send")?;
    let tx_hash_string = hex::encode_prefixed(tx_hash);
    // Executor payloads already reserve inputs durably, without the ordinary
    // local TTL. Avoid a second lock that would outlive a recovery winner.
    if executor.is_none() {
        session
            .mark_pending_spent_utxos(
                pending_spent_inputs,
                parse_submitted_tx_hash(&tx_hash_string),
            )
            .await;
    }
    tracing::info!(
        target: LOG_TARGET,
        providers = provider_handles.len(),
        "sent self-broadcast transaction"
    );
    Ok(())
}

pub(crate) async fn self_broadcast_send_raw_transaction_to_rpc_pool(
    query_rpc_pool: &QueryRpcPool,
    network_mode: WalletNetworkMode,
    signed_tx: Vec<u8>,
    tx_hash: FixedBytes<32>,
) -> Result<Vec<ProviderHandle>> {
    self_broadcast_send_raw_transaction_to_rpc_pool_with_logging(
        query_rpc_pool,
        network_mode,
        signed_tx,
        tx_hash,
        true,
    )
    .await
}

pub(crate) async fn self_broadcast_send_raw_transaction_to_rpc_pool_with_logging(
    query_rpc_pool: &QueryRpcPool,
    network_mode: WalletNetworkMode,
    signed_tx: Vec<u8>,
    tx_hash: FixedBytes<32>,
    log_transaction_details: bool,
) -> Result<Vec<ProviderHandle>> {
    let providers = query_rpc_pool.available_providers();
    if providers.is_empty() {
        return Err(eyre!("no healthy query RPC available"));
    }

    let policy = SelfBroadcastFeeQuoteTimeoutPolicy::for_network_mode(network_mode);
    let started_at = Instant::now();
    let mut join_set = JoinSet::new();
    for provider_handle in providers {
        join_set.spawn(self_broadcast_send_raw_transaction_to_provider(
            provider_handle,
            signed_tx.clone(),
            tx_hash,
            policy.hard_deadline,
        ));
    }

    let mut accepted_provider_handles = Vec::new();
    let mut last_error = None;
    let mut grace_deadline = None;
    while !join_set.is_empty() {
        let now = Instant::now();
        let Some(hard_remaining) = policy
            .hard_deadline
            .checked_sub(now.saturating_duration_since(started_at))
        else {
            break;
        };
        let wait_for = grace_deadline.map_or(hard_remaining, |deadline: Instant| {
            deadline.saturating_duration_since(now).min(hard_remaining)
        });
        if wait_for.is_zero() {
            break;
        }

        match tokio::time::timeout(wait_for, join_set.join_next()).await {
            Ok(Some(Ok(result))) => match result.outcome {
                SelfBroadcastRawTxBroadcastOutcome::Accepted => {
                    if log_transaction_details {
                        let url = crate::http::redact_url_for_display(&result.provider_handle.url);
                        tracing::info!(
                            target: LOG_TARGET,
                            %url,
                            "self-broadcast tx accepted by RPC"
                        );
                    }
                    accepted_provider_handles.push(result.provider_handle);
                    if grace_deadline.is_none() {
                        grace_deadline = Some(Instant::now() + policy.grace_after_first_usable);
                    }
                }
                SelfBroadcastRawTxBroadcastOutcome::AlreadyKnown => {
                    if log_transaction_details {
                        let url = crate::http::redact_url_for_display(&result.provider_handle.url);
                        tracing::info!(
                            target: LOG_TARGET,
                            %url,
                            "self-broadcast tx already known by RPC"
                        );
                    }
                    accepted_provider_handles.push(result.provider_handle);
                    if grace_deadline.is_none() {
                        grace_deadline = Some(Instant::now() + policy.grace_after_first_usable);
                    }
                }
                SelfBroadcastRawTxBroadcastOutcome::Rejected(message) => {
                    if log_transaction_details {
                        let url = crate::http::redact_url_for_display(&result.provider_handle.url);
                        tracing::warn!(
                            target: LOG_TARGET,
                            %url,
                            message,
                            "self-broadcast tx rejected by RPC"
                        );
                    }
                    last_error = Some(if log_transaction_details {
                        message
                    } else {
                        "RPC rejected dapp transaction".to_owned()
                    });
                }
            },
            Ok(Some(Err(error))) => {
                last_error = Some(if log_transaction_details {
                    error.to_string()
                } else {
                    "Dapp transaction broadcast task failed".to_owned()
                });
            }
            Ok(None) | Err(_) => break,
        }
    }
    join_set.abort_all();

    if accepted_provider_handles.is_empty() {
        return Err(eyre!(last_error.unwrap_or_else(|| {
            "self-broadcast transaction was not accepted by any RPC before the deadline".to_string()
        })));
    }
    Ok(accepted_provider_handles)
}

pub(in crate::desktop) async fn self_broadcast_send_raw_transaction_to_provider(
    provider_handle: ProviderHandle,
    signed_tx: Vec<u8>,
    tx_hash: FixedBytes<32>,
    timeout: Duration,
) -> SelfBroadcastRawTxBroadcastResult {
    let send_result = tokio::time::timeout(
        timeout,
        provider_handle.provider.send_raw_transaction(&signed_tx),
    )
    .await;
    let outcome = match send_result {
        Ok(Ok(pending)) => {
            let returned_hash = pending.tx_hash().to_owned();
            if returned_hash == tx_hash {
                SelfBroadcastRawTxBroadcastOutcome::Accepted
            } else {
                SelfBroadcastRawTxBroadcastOutcome::Rejected(
                    "RPC returned an unexpected transaction hash".to_owned(),
                )
            }
        }
        Ok(Err(error)) if is_self_broadcast_tx_already_known_error(&error) => {
            SelfBroadcastRawTxBroadcastOutcome::AlreadyKnown
        }
        Ok(Err(error)) => SelfBroadcastRawTxBroadcastOutcome::Rejected(error.to_string()),
        Err(_) => SelfBroadcastRawTxBroadcastOutcome::Rejected(
            "self-broadcast send timed out".to_string(),
        ),
    };
    SelfBroadcastRawTxBroadcastResult {
        provider_handle,
        outcome,
    }
}

pub(in crate::desktop) fn is_self_broadcast_tx_already_known_error(
    error: &alloy::transports::TransportError,
) -> bool {
    error
        .as_error_resp()
        .is_some_and(|response| is_self_broadcast_tx_already_known_message(&response.message))
}

pub(crate) fn is_self_broadcast_tx_already_known_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("already known")
        || message.contains("already in mempool")
        || message.contains("known transaction")
        || message.contains("already imported")
        || message.contains("already have")
        || message.contains("already exists")
        || message.contains("transaction already")
}

pub(in crate::desktop) async fn mark_submitted_inputs_pending_spent(
    session: &WalletSession,
    inputs: &[Utxo],
    result: &PublicBroadcasterSubmissionResult,
) {
    let PublicBroadcasterResultKind::Submitted { tx_hash } = &result.result else {
        return;
    };
    session
        .mark_pending_spent_utxos(inputs, Some(*tx_hash))
        .await;
}

pub(crate) fn parse_submitted_tx_hash(tx_hash: &str) -> Option<FixedBytes<32>> {
    tx_hash.parse().ok()
}
