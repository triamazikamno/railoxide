use super::*;

pub async fn submit_desktop_unshield_public_broadcaster(
    mut request: DesktopUnshieldPublicBroadcasterRequest,
    http: &HttpContext,
) -> Result<PublicBroadcasterSubmissionResult> {
    let waku = request.waku.clone();
    let timeout = request.response_timeout;
    let republish_interval = request.republish_interval;
    let progress_tx = request.progress_tx.clone();
    let route_tx = request.route_tx.clone();
    let payload_inputs_tx = request.payload_inputs_tx.take();
    let session = Arc::clone(&request.session);
    let (vault_store, view_session) = (
        Arc::clone(&request.vault_store),
        Arc::clone(&request.view_session),
    );
    let (chain_id, token, amount, recipient) = (
        request.chain_id,
        request.token,
        request.amount,
        request.recipient,
    );
    let gateway_origin = request.gateway_origin.clone();
    let prepared = Box::pin(prepare_desktop_unshield_public_broadcaster(request, http)).await?;
    let pending_spent_inputs = prepared.plan.input_utxos();
    let payload_inputs = match &prepared.plan {
        DesktopUnshieldPreparedPlan::Single(plan) => single_payload_inputs(
            plan.chunks
                .iter()
                .map(|chunk| chunk.inputs.iter().map(|input| &input.utxo)),
            token,
        ),
        DesktopUnshieldPreparedPlan::Composite(_) => None,
    };
    let transaction = prepared.transaction.unwrap_or_else(|| {
        TransactionRequest::default()
            .to(prepared.plan.call_to())
            .input(prepared.plan.call_data().into())
    });
    let executor = if let Some(owner) = session.executor_owner() {
        owner
            .transaction_identity(&transaction)?
            .map(|identity| (owner, identity))
    } else {
        None
    };
    let activity = ActivitySubmission::private(
        &vault_store,
        &view_session,
        PrivateActivity {
            chain_id,
            kind: vault::ActivityKind::Unshield,
            token,
            amount,
            counterparty: vault::ActivityCounterparty::Evm(recipient),
            fee: Some(vault::ActivityFee {
                asset: vault::ActivityAsset::Token(prepared.fee_token),
                amount: prepared.fee_amount,
            }),
            gas_payer: None,
            inputs: activity_inputs(&pending_spent_inputs),
        },
        executor.as_ref().is_some_and(|(owner, identity)| {
            is_swap_step(&owner.records().unwrap_or_default(), identity.operation())
        }),
    )
    .with_gateway_origin(gateway_origin);
    if let (Some(tx), Some(inputs)) = (payload_inputs_tx, &payload_inputs) {
        let _ = tx.send(inputs.clone());
    }
    activity.begin();
    let result = submit_public_broadcaster_plan(
        &waku,
        transaction,
        prepared.pre_transaction_pois_per_txid_leaf_per_list,
        prepared.broadcaster,
        prepared.action_token,
        prepared.fee_token,
        prepared.entered_amount,
        prepared.receiver_amount,
        prepared.recipient_amount,
        prepared.total_private_spend,
        prepared.fee_amount,
        prepared.protocol_fee_amount,
        prepared.protocol_fee_bps,
        prepared.fee_mode,
        prepared.gas_limit,
        prepared.min_gas_price,
        prepared.bound_min_gas_price,
        prepared.transaction_count,
        prepared.input_count,
        prepared.private_output_count,
        prepared.public_output_count,
        prepared.relay_call_count,
        prepared.uses_relay_adapt,
        prepared.native_top_up,
        payload_inputs,
        progress_tx,
        route_tx,
        timeout,
        republish_interval,
    )
    .await;
    activity.finish_relay(result.as_ref().ok().map(|result| &result.result));
    let result = result?;
    if let Some((owner, identity)) = &executor
        && let PublicBroadcasterResultKind::Submitted { tx_hash } = &result.result
    {
        owner.record_submission(*identity, tx_hash.parse()?)?;
    }
    if executor.is_none() {
        mark_submitted_inputs_pending_spent(&session, &pending_spent_inputs, &result).await;
    }
    Ok(result)
}

pub async fn submit_desktop_send_public_broadcaster(
    mut request: DesktopSendPublicBroadcasterRequest,
    http: &HttpContext,
) -> Result<PublicBroadcasterSubmissionResult> {
    let waku = request.waku.clone();
    let timeout = request.response_timeout;
    let republish_interval = request.republish_interval;
    let progress_tx = request.progress_tx.clone();
    let route_tx = request.route_tx.clone();
    let payload_inputs_tx = request.payload_inputs_tx.take();
    let session = Arc::clone(&request.session);
    let (vault_store, view_session) = (
        Arc::clone(&request.vault_store),
        Arc::clone(&request.view_session),
    );
    let (chain_id, token, amount) = (request.chain_id, request.token, request.amount);
    let recipient = request.recipient.trim().to_owned();
    let gateway_origin = request.gateway_origin.clone();
    let prepared = prepare_desktop_send_public_broadcaster(request, http).await?;
    let pending_spent_inputs = prepared
        .plan
        .inputs
        .iter()
        .map(|input| input.utxo.clone())
        .collect::<Vec<_>>();
    let payload_inputs = single_payload_inputs(
        prepared
            .plan
            .chunks
            .iter()
            .map(|chunk| chunk.inputs.iter().map(|input| &input.utxo)),
        token,
    );
    let activity = ActivitySubmission::private(
        &vault_store,
        &view_session,
        PrivateActivity {
            chain_id,
            kind: vault::ActivityKind::PrivateSend,
            token,
            amount,
            counterparty: vault::ActivityCounterparty::Private(recipient),
            fee: Some(vault::ActivityFee {
                asset: vault::ActivityAsset::Token(prepared.fee_token),
                amount: prepared.fee_amount,
            }),
            gas_payer: None,
            inputs: activity_inputs(&pending_spent_inputs),
        },
        false,
    )
    .with_gateway_origin(gateway_origin);
    if let (Some(tx), Some(inputs)) = (payload_inputs_tx, &payload_inputs) {
        let _ = tx.send(inputs.clone());
    }
    activity.begin();
    let result = submit_public_broadcaster_plan(
        &waku,
        prepared.transaction.unwrap_or_else(|| {
            TransactionRequest::default()
                .to(prepared.plan.call.to)
                .input(prepared.plan.call.data.into())
        }),
        prepared.pre_transaction_pois_per_txid_leaf_per_list,
        prepared.broadcaster,
        prepared.action_token,
        prepared.fee_token,
        prepared.entered_amount,
        prepared.receiver_amount,
        prepared.recipient_amount,
        prepared.total_private_spend,
        prepared.fee_amount,
        prepared.protocol_fee_amount,
        prepared.protocol_fee_bps,
        prepared.fee_mode,
        prepared.gas_limit,
        prepared.min_gas_price,
        prepared.bound_min_gas_price,
        prepared.transaction_count,
        prepared.input_count,
        prepared.private_output_count,
        prepared.public_output_count,
        prepared.relay_call_count,
        prepared.uses_relay_adapt,
        prepared.native_top_up,
        payload_inputs,
        progress_tx,
        route_tx,
        timeout,
        republish_interval,
    )
    .await;
    activity.finish_relay(result.as_ref().ok().map(|result| &result.result));
    let result = result?;
    mark_submitted_inputs_pending_spent(&session, &pending_spent_inputs, &result).await;
    Ok(result)
}

pub async fn submit_desktop_unshield_self_broadcast(
    request: DesktopUnshieldSelfBroadcastRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<DesktopSelfBroadcastResult> {
    if let Some(prepared) = &request.executor {
        if request.executor_maximum_gas.is_none_or(|gas| gas == 0)
            || matches!(request.gas_fee, SelfBroadcastGasFeeSelection::Auto)
        {
            return Err(eyre!(
                "review the executor gas limit and fee values before proving this operation"
            ));
        }
        let payer = self_broadcast_gas_payer(
            &request.vault_store,
            &request.view_session,
            &request.public_account_uuid,
        )?;
        if !matches!(prepared.delivery(), ExecutorDelivery::SelfBroadcast { sender, sponsored: false } if *sender == payer)
        {
            return Err(eyre!(
                "executor preparation selected another gas payer or funding route"
            ));
        }
    }
    let prepared = prepare_desktop_unshield_plan_without_broadcaster_fee(
        DesktopUnshieldPlanRequest {
            executor: request
                .executor
                .as_deref()
                .map(|prepared| (prepared, client)),
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
    let pending_output_pois_required =
        unshield_chunks_require_pending_output_pois(prepared.plan.chunks());
    emit_self_broadcast_event(
        request.event_tx.as_ref(),
        SelfBroadcastSessionEvent::PendingOutputPoiProofsRequired {
            required: pending_output_pois_required,
        },
    );
    if pending_output_pois_required {
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
            "generate self-broadcast unshield pending output pre-transaction POI",
        )
        .await?;
    }
    let pending_spent_inputs = prepared.plan.input_utxos();
    let mut transaction = prepared.transaction.unwrap_or_else(|| {
        TransactionRequest::default()
            .to(prepared.plan.call_to())
            .input(prepared.plan.call_data().into())
    });
    if request.executor.is_some() {
        transaction.gas = request.executor_maximum_gas;
    }
    let activity = ActivitySubmission::private(
        &request.vault_store,
        &request.view_session,
        PrivateActivity {
            chain_id: request.chain_id,
            kind: vault::ActivityKind::Unshield,
            token: request.token,
            amount: request.amount,
            counterparty: vault::ActivityCounterparty::Evm(request.recipient),
            fee: None,
            gas_payer: None,
            inputs: activity_inputs(&pending_spent_inputs),
        },
        request.executor.as_ref().is_some_and(|executor| {
            request
                .session
                .executor_owner()
                .and_then(|owner| owner.records().ok())
                .is_some_and(|records| is_swap_step(&records, executor.operation()))
        }),
    )
    .with_gateway_origin(request.gateway_origin);
    let mut result = submit_self_broadcast_plan(
        Some(&activity),
        request.transaction_tracking.as_ref(),
        request.chain_id,
        &request.effective_chain,
        request.view_session.as_ref(),
        request.vault_store.as_ref(),
        request
            .vault_password
            .as_ref()
            .map(|password| password.as_str()),
        request.protected_software_seed_session.as_deref(),
        Some(request.spend_authorization),
        request.trezor_pin_matrix_provider,
        request.public_account_uuid,
        Arc::clone(&request.session),
        transaction,
        pending_spent_inputs,
        prepared.native_top_up.is_some(),
        request.gas_fee,
        request.progress_tx,
        request.command_rx,
        request.event_tx,
        http,
        client,
    )
    .await?;
    result.native_top_up = prepared.native_top_up;
    Ok(result)
}

pub async fn submit_blocked_shield_rescue_self_broadcast(
    request: BlockedShieldRescueSelfBroadcastRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<DesktopSelfBroadcastResult> {
    let prepared = prepare_blocked_shield_rescue_plan(&request, http, client).await?;
    let pending_output_pois_required =
        unshield_chunks_require_pending_output_pois(&prepared.plan.chunks);
    emit_self_broadcast_event(
        request.event_tx.as_ref(),
        SelfBroadcastSessionEvent::PendingOutputPoiProofsRequired {
            required: pending_output_pois_required,
        },
    );
    if pending_output_pois_required {
        return Err(eyre!(
            "blocked Shield refund plan unexpectedly requires private output POI proofs"
        ));
    }
    let pending_spent_inputs = prepared
        .plan
        .inputs
        .iter()
        .map(|input| input.utxo.clone())
        .collect::<Vec<_>>();
    let activity = ActivitySubmission::private(
        &request.vault_store,
        &request.view_session,
        PrivateActivity {
            chain_id: request.chain_id,
            kind: vault::ActivityKind::Refund,
            token: prepared.token,
            amount: prepared.plan.unshield_note.value,
            counterparty: vault::ActivityCounterparty::Evm(prepared.origin_address),
            fee: None,
            gas_payer: None,
            inputs: activity_inputs(&pending_spent_inputs),
        },
        false,
    );
    submit_self_broadcast_plan(
        Some(&activity),
        request.transaction_tracking.as_ref(),
        request.chain_id,
        &request.effective_chain,
        request.view_session.as_ref(),
        request.vault_store.as_ref(),
        request
            .vault_password
            .as_ref()
            .map(|password| password.as_str()),
        request.protected_software_seed_session.as_deref(),
        Some(request.spend_authorization),
        request.trezor_pin_matrix_provider,
        prepared.public_account_uuid,
        Arc::clone(&request.session),
        TransactionRequest::default()
            .to(prepared.plan.call.to)
            .input(prepared.plan.call.data.into()),
        pending_spent_inputs,
        false,
        request.gas_fee,
        request.progress_tx,
        request.command_rx,
        request.event_tx,
        http,
        client,
    )
    .await
}

pub async fn submit_desktop_send_self_broadcast(
    request: DesktopSendSelfBroadcastRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<DesktopSelfBroadcastResult> {
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
        "generate self-broadcast send pending output pre-transaction POI",
    )
    .await?;
    let pending_spent_inputs = prepared
        .plan
        .inputs
        .iter()
        .map(|input| input.utxo.clone())
        .collect::<Vec<_>>();
    let activity = ActivitySubmission::private(
        &request.vault_store,
        &request.view_session,
        PrivateActivity {
            chain_id: request.chain_id,
            kind: vault::ActivityKind::PrivateSend,
            token: request.token,
            amount: request.amount,
            counterparty: vault::ActivityCounterparty::Private(recipient),
            fee: None,
            gas_payer: None,
            inputs: activity_inputs(&pending_spent_inputs),
        },
        false,
    )
    .with_gateway_origin(request.gateway_origin);
    submit_self_broadcast_plan(
        Some(&activity),
        request.transaction_tracking.as_ref(),
        request.chain_id,
        &request.effective_chain,
        request.view_session.as_ref(),
        request.vault_store.as_ref(),
        request
            .vault_password
            .as_ref()
            .map(|password| password.as_str()),
        request.protected_software_seed_session.as_deref(),
        Some(request.spend_authorization),
        request.trezor_pin_matrix_provider,
        request.public_account_uuid,
        Arc::clone(&request.session),
        TransactionRequest::default()
            .to(prepared.plan.call.to)
            .input(prepared.plan.call.data.into()),
        pending_spent_inputs,
        false,
        request.gas_fee,
        request.progress_tx,
        request.command_rx,
        request.event_tx,
        http,
        client,
    )
    .await
}
