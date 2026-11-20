use super::*;

pub(in crate::desktop) async fn prepare_desktop_send_public_broadcaster(
    request: DesktopSendPublicBroadcasterRequest,
    http: &HttpContext,
) -> Result<PreparedPublicBroadcasterPlan<SendPlan>> {
    if request.session.chain_id != request.chain_id {
        return Err(eyre!(
            "selected wallet session is for chain {}, not {}",
            request.session.chain_id,
            request.chain_id
        ));
    }
    if request.custom_fee_amount.is_none() && request.maximum_private_fee.is_none() {
        return Err(eyre!(
            "review the broadcaster fee limit before proving this operation"
        ));
    }

    let recipient = parse_railgun_recipient(&request.recipient)?;
    let PublicBroadcasterSetup {
        chain,
        broadcaster,
        query_rpc_pool,
        min_gas_price,
        prover,
        forest,
        utxos,
    } = public_broadcaster_setup(
        &request.session,
        request.chain_id,
        &request.effective_chain,
        request.fee_token,
        &request.quote.broadcaster,
        None,
        request.fee_policy,
        &request.trust_filter,
        request.anchor_cache.as_ref(),
        http,
        &http.operation_http_client().await?,
    )
    .await?;
    let pinned = PublicBroadcasterInputPin::active(request.input_pin.as_ref());
    let bound_min_gas_price =
        public_broadcaster_bound_min_gas_price(request.chain_id, min_gas_price);
    let same_token_fee = request.fee_token == request.token;
    update_transaction_generation_stage(
        request.progress_tx.as_ref(),
        TransactionGenerationStage::SelectingPrivateNotes,
    );
    let seeded_fee_amount = initial_public_broadcaster_fee_amount(
        &broadcaster,
        chain.gas.gas_limit_buffer,
        min_gas_price,
        same_token_fee,
        || {
            let selection = send_selection_info_with_separate_broadcaster_fee_seed(
                &utxos,
                request.token,
                request.fee_token,
                request.amount,
                false,
            )
            .map_err(|error| {
                public_broadcaster_build_error(
                    error,
                    U256::ZERO,
                    FeeHandlingMode::AddToAmount,
                    same_token_fee,
                    U256::ZERO,
                )
            })?;
            Ok(send_approximate_shape(&selection, selection.max_spendable))
        },
    )?;
    let initial_fee_estimate = initial_public_broadcaster_fee_estimate(
        "send",
        approximate_public_broadcaster_cost(
            broadcaster.clone(),
            chain.gas.gas_limit_buffer,
            request.token,
            request.fee_token,
            request.amount,
            request.fee_mode,
            U256::ZERO,
            min_gas_price,
            seeded_fee_amount,
            request.custom_fee_amount,
            |split| {
                let selection = send_selection_info_with_broadcaster_fee_token(
                    &utxos,
                    request.token,
                    request.fee_token,
                    split.receiver_amount,
                    split.fee_amount,
                    false,
                )
                .map_err(|error| {
                    public_broadcaster_build_error(
                        error,
                        split.fee_amount,
                        split.fee_mode,
                        same_token_fee,
                        U256::ZERO,
                    )
                })?;
                Ok(send_approximate_shape(&selection, selection.max_spendable))
            },
        ),
        same_token_fee && request.custom_fee_amount.is_none(),
        &broadcaster,
        min_gas_price,
        bound_min_gas_price,
    )?;

    let signer = request.spend_authorization.into_signer(
        request.vault_store.as_ref(),
        request.view_session.as_ref(),
        "public broadcaster send",
    )?;

    let tx_builder = TransactionBuilder {
        chain_type: 0,
        chain_id: request.chain_id,
        railgun_contract: chain.railgun_contract,
    };

    let maximum_fee = request.maximum_private_fee;
    let mut fee_amount = starting_public_broadcaster_fee(
        request.custom_fee_amount,
        maximum_fee,
        initial_fee_estimate.as_ref(),
    )?;
    for attempt in 1..=PUBLIC_BROADCASTER_FEE_ATTEMPTS {
        let split = public_broadcaster_amount_split_for_tokens(
            request.amount,
            fee_amount,
            request.fee_mode,
            same_token_fee,
        )?;
        let send_request = RailgunSendRequest {
            token_address: request.token,
            amount: split.receiver_amount,
            recipient,
            verify_proof: request.verify_proof,
            spend_up_to: false,
            broadcaster_fee: Some(BroadcasterFeeOutput {
                recipient: broadcaster.address_data,
                token_address: request.fee_token,
                amount: fee_amount,
            }),
            min_gas_price: bound_min_gas_price,
        };
        update_transaction_generation_stage(
            request.progress_tx.as_ref(),
            TransactionGenerationStage::ProvingTransaction,
        );
        let proof_started = Instant::now();
        let plan = tx_builder
            .build_send_plan_with_signer(
                &request.view_session.scan_keys(),
                &signer,
                &forest,
                &utxos,
                send_request,
                &prover,
            )
            .await
            .map_err(|error| {
                public_broadcaster_build_error(
                    error,
                    fee_amount,
                    split.fee_mode,
                    same_token_fee,
                    U256::ZERO,
                )
            })
            .wrap_err("build public broadcaster send proof")?;
        require_pinned_payload(
            pinned,
            single_payload_inputs(
                plan.chunks
                    .iter()
                    .map(|chunk| chunk.inputs.iter().map(|input| &input.utxo)),
                request.token,
            )
            .as_deref(),
        )?;
        let chunk_input_counts = plan
            .chunks
            .iter()
            .map(|chunk| chunk.inputs.len())
            .collect::<Vec<_>>();
        let chunk_output_counts = plan
            .chunks
            .iter()
            .map(|chunk| chunk.outputs.len())
            .collect::<Vec<_>>();
        let chunk_tree_numbers = plan
            .chunks
            .iter()
            .map(|chunk| chunk.tree_number)
            .collect::<Vec<_>>();
        tracing::info!(target: LOG_TARGET,
            attempt,
            fee_amount = %fee_amount,
            elapsed_ms = proof_started.elapsed().as_millis(),
            transaction_count = plan.transaction_count(),
            input_count = plan.input_count(),
            private_output_count = plan.private_output_count(),
            public_output_count = plan.public_output_count(),
            same_token_fee,
            ?chunk_input_counts,
            ?chunk_output_counts,
            ?chunk_tree_numbers,
            broadcaster = %broadcaster.railgun_address,
            fees_id = %broadcaster.fees_id,
            "built public broadcaster send proof"
        );
        update_transaction_generation_stage(
            request.progress_tx.as_ref(),
            TransactionGenerationStage::EstimatingBroadcasterFee,
        );
        let gas_started = Instant::now();
        let (gas_limit, computed_fee) = estimate_public_broadcaster_fee_from_rpc_pool(
            &query_rpc_pool,
            request.chain_id,
            TransactionRequest::default()
                .to(plan.call.to)
                .input(plan.call.data.clone().into()),
            broadcaster.fee,
            min_gas_price,
            chain.gas.gas_limit_buffer,
        )
        .await?;
        let gas_elapsed_ms = gas_started.elapsed().as_millis();
        tracing::info!(target: LOG_TARGET,
            attempt,
            available_fee = %fee_amount,
            computed_fee = %computed_fee,
            gas_limit,
            min_gas_price,
            bound_min_gas_price,
            gas_elapsed_ms,
            broadcaster = %broadcaster.railgun_address,
            fees_id = %broadcaster.fees_id,
            "estimated public broadcaster send fee"
        );
        if broadcaster_fee_covers(fee_amount, computed_fee) {
            let protocol_fee_amount = U256::ZERO;
            tracing::info!(target: LOG_TARGET,
                attempt,
                fee_amount = %fee_amount,
                computed_fee = %computed_fee,
                gas_limit,
                broadcaster = %broadcaster.railgun_address,
                fees_id = %broadcaster.fees_id,
                "public broadcaster send fee stabilized"
            );
            update_transaction_generation_stage(
                request.progress_tx.as_ref(),
                TransactionGenerationStage::GeneratingPoiProofs,
            );
            let pre_transaction_pois = public_broadcaster_pre_transaction_pois(
                &plan.chunks,
                &broadcaster,
                request.session.as_ref(),
                request.chain_id,
                &prover,
                request.verify_proof,
                http,
            )
            .await?;
            let pending_persist_started = Instant::now();
            let pending_contexts = persist_pending_send_output_poi_contexts(
                request.session.as_ref(),
                &plan.chunks,
                &pre_transaction_pois.pending_pois,
                &pre_transaction_pois.pending_poi_list_keys,
                true,
                !same_token_fee,
            )
            .await?;
            tracing::info!(target: LOG_TARGET,
                chain_id = request.chain_id,
                pending_contexts,
                elapsed_ms = pending_persist_started.elapsed().as_millis(),
                "persisted public broadcaster send pending output POI contexts"
            );
            return Ok(PreparedPublicBroadcasterPlan {
                transaction: None,
                transaction_count: plan.transaction_count(),
                input_count: plan.input_count(),
                private_output_count: plan.private_output_count(),
                public_output_count: plan.public_output_count(),
                relay_call_count: 0,
                uses_relay_adapt: false,
                plan,
                pre_transaction_pois_per_txid_leaf_per_list: pre_transaction_pois.request_pois,
                broadcaster,
                action_token: request.token,
                fee_token: request.fee_token,
                entered_amount: split.entered_amount,
                receiver_amount: split.receiver_amount,
                recipient_amount: recipient_amount_after_protocol_fee(
                    split.receiver_amount,
                    protocol_fee_amount,
                ),
                total_private_spend: split.total_private_spend,
                fee_amount,
                protocol_fee_amount,
                protocol_fee_bps: U256::ZERO,
                fee_mode: split.fee_mode,
                gas_limit,
                min_gas_price,
                bound_min_gas_price,
                native_top_up: None,
            });
        }
        let next_fee = request.custom_fee_amount.map_or_else(
            || bounded_public_broadcaster_fee(computed_fee, maximum_fee),
            |amount| validate_custom_public_broadcaster_fee(amount, computed_fee, maximum_fee),
        )?;
        log_public_broadcaster_fee_prediction_failure(
            "send",
            attempt,
            fee_amount,
            computed_fee,
            gas_limit,
            initial_fee_estimate.as_ref(),
            plan.transaction_count(),
            plan.input_count(),
            plan.private_output_count(),
            plan.public_output_count(),
            &broadcaster,
        );
        tracing::info!(target: LOG_TARGET,
            attempt,
            previous_fee = %fee_amount,
            computed_fee = %computed_fee,
            next_fee = %next_fee,
            "retrying public broadcaster send proof with buffered fee"
        );
        fee_amount = next_fee;
    }

    Err(eyre!(
        "public broadcaster fee did not stabilize after bounded retries"
    ))
}
