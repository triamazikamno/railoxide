#[allow(clippy::wildcard_imports)]
use super::*;

pub async fn submit_public_send(
    request: PublicSendRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<PublicSendResult> {
    submit_public_send_with_progress(request, http, client, |_| {}).await
}

pub async fn submit_public_send_with_progress(
    request: PublicSendRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
    mut progress: impl FnMut(PublicActionProgressUpdate) + Send,
) -> Result<PublicSendResult> {
    validate_public_transaction_intent(&request.intent)?;
    validate_public_wrapped_native_target(&request.intent, &request.effective_chain)?;
    let (step, operation_label, revert_subject) = match request.intent {
        PublicTransactionIntent::WrapNative { .. } => (
            PublicActionProgressStep::Wrap,
            "public-wrap",
            "public wrap transaction",
        ),
        PublicTransactionIntent::UnwrapNative { .. } => (
            PublicActionProgressStep::Unwrap,
            "public-unwrap",
            "public unwrap transaction",
        ),
        PublicTransactionIntent::Transfer { .. } | PublicTransactionIntent::Raw { .. } => (
            PublicActionProgressStep::Send,
            "public-send",
            "public send transaction",
        ),
    };
    let activity = ActivitySubmission::public(
        &request.vault_store,
        &request.view_session,
        public_transaction_activity(
            &request.public_account_uuid,
            request.chain_id,
            &request.intent,
        ),
    )
    .with_gateway_origin(request.gateway_origin.clone());
    let signer = admitted_public_signer_authorized(
        &request.vault_store,
        &request.view_session,
        request.authorization.as_ref(),
        &request.public_account_uuid,
        request.trezor_app_passphrase,
        request.trezor_pin_matrix_provider,
        request.executor_owner.as_ref(),
        request.chain_id,
        client,
    )
    .await?;
    signer
        .while_active(async {
            let mut command_rx = request.command_rx;
            let tx = submit_public_action_step_with_signer(
                step,
                operation_label,
                revert_subject,
                request.chain_id,
                &request.effective_chain,
                &request.intent,
                &signer,
                request.advanced_authorization,
                request.reviewed_gas_limit,
                false,
                request.gas_fee,
                &mut command_rx,
                request.event_tx.as_ref(),
                http,
                client,
                request.transaction_tracking.as_ref(),
                Some((&activity, ActivityStep::Main)),
                &mut progress,
            )
            .await?;
            Ok(PublicSendResult { tx })
        })
        .await
}

pub(in crate::public_wallet) fn public_send_authorized_gas_limit(
    chain_id: u64,
    from: Address,
    intent: &PublicTransactionIntent,
    authorization: Option<crate::public_wallet::types::PublicAdvancedTransactionAuthorization>,
    gas_fee: crate::public_wallet::types::PublicActionGasFeeSelection,
) -> Result<Option<u64>> {
    match intent {
        PublicTransactionIntent::Transfer { .. }
        | PublicTransactionIntent::WrapNative { .. }
        | PublicTransactionIntent::UnwrapNative { .. } => {
            if authorization.is_some() {
                return Err(eyre!(
                    "advanced transaction authorization cannot be used for a transfer"
                ));
            }
            Ok(None)
        }
        PublicTransactionIntent::Raw { .. } => {
            let authorization =
                authorization.ok_or_else(|| eyre!("advanced transaction estimate is required"))?;
            if authorization.gas_limit == 0 {
                return Err(eyre!(
                    "authorized advanced gas limit must be greater than zero"
                ));
            }
            let crate::public_wallet::types::PublicActionGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            } = gas_fee
            else {
                return Err(eyre!(
                    "advanced transaction submission requires the estimated gas fee values"
                ));
            };
            let fingerprint = public_advanced_transaction_payload_fingerprint(
                chain_id,
                from,
                intent,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            );
            if fingerprint != authorization.payload_fingerprint {
                return Err(eyre!(
                    "advanced transaction payload changed after gas estimation"
                ));
            }
            Ok(Some(authorization.gas_limit))
        }
    }
}

pub(in crate::public_wallet) fn public_send_transaction_request(
    chain_id: u64,
    from: Address,
    intent: &PublicTransactionIntent,
) -> Result<TransactionRequest> {
    validate_public_transaction_intent(intent)?;
    let mut tx_req = TransactionRequest::default()
        .with_chain_id(chain_id)
        .with_from(from);
    match intent {
        PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Native,
            amount,
            recipient,
        } => {
            tx_req = tx_req.with_to(*recipient).with_value(*amount);
        }
        PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Erc20(token),
            amount,
            recipient,
        } => {
            tx_req = tx_req.with_to(*token).with_input(
                PublicErc20::transferCall {
                    recipient: *recipient,
                    amount: *amount,
                }
                .abi_encode(),
            );
        }
        PublicTransactionIntent::Raw { to, value, data } => {
            match to {
                Some(to) => tx_req = tx_req.with_to(*to),
                None => tx_req = tx_req.create(),
            }
            tx_req = tx_req.with_value(*value).with_input(data.clone());
        }
        PublicTransactionIntent::WrapNative { wrapped, amount } => {
            tx_req = tx_req
                .with_to(*wrapped)
                .with_value(*amount)
                .with_input(PublicWrappedNative::depositCall {}.abi_encode());
        }
        PublicTransactionIntent::UnwrapNative { wrapped, amount } => {
            tx_req = tx_req
                .with_to(*wrapped)
                .with_input(PublicWrappedNative::withdrawCall { amount: *amount }.abi_encode());
        }
    }
    Ok(tx_req)
}
