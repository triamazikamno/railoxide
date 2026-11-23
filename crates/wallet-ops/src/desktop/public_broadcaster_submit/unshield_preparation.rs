use super::*;

pub(in crate::desktop) async fn prepare_desktop_unshield_public_broadcaster(
    request: DesktopUnshieldPublicBroadcasterRequest,
    http: &HttpContext,
) -> Result<PreparedPublicBroadcasterPlan<DesktopUnshieldPreparedPlan>> {
    if request.session.chain_id != request.chain_id {
        return Err(eyre!(
            "selected wallet session is for chain {}, not {}",
            request.session.chain_id,
            request.chain_id
        ));
    }
    require_unshield_executor(
        request.unwrap,
        request.native_top_up.is_some(),
        request.executor.is_some(),
    )?;
    let pinned = PublicBroadcasterInputPin::active(request.input_pin.as_ref());
    // Executor retries are bound by their operation's nonce instead; a pin can't apply to them.
    if pinned.is_some() && request.executor.is_some() {
        return Err(PublicBroadcasterInputPinUnavailable.into());
    }
    let chain = effective_desktop_chain_config(request.chain_id, &request.effective_chain)?;
    let executor =
        validate_desktop_executor_preparation(&request.session, request.executor.as_deref())?;
    if executor.is_some() && request.maximum_private_fee.is_none() {
        return Err(eyre!(
            "review the executor fee limit before proving this operation"
        ));
    }
    if request.custom_fee_amount.is_none() && request.maximum_private_fee.is_none() {
        return Err(eyre!(
            "review the broadcaster fee limit before proving this operation"
        ));
    }
    if request.unwrap && !is_effective_wrapped_native_token(request.chain_id, request.token, &chain)
    {
        return Err(eyre!("selected token does not support unwrap-to-native"));
    }

    // One client for the setup's reads and the executor's issuance reads.
    let client = http.operation_http_client().await?;
    let PublicBroadcasterSetup {
        chain,
        broadcaster,
        query_rpc_pool,
        min_gas_price,
        prover,
        forest,
        mut utxos,
    } = public_broadcaster_setup(
        &request.session,
        request.chain_id,
        &request.effective_chain,
        request.fee_token,
        &request.quote.broadcaster,
        request
            .executor
            .as_ref()
            .map(|prepared| prepared.delivery()),
        request.fee_policy,
        &request.trust_filter,
        request.anchor_cache.as_ref(),
        http,
        &client,
    )
    .await?;
    if let Some(prepared) = &request.executor {
        prepared.require_broadcaster(&broadcaster)?;
        utxos = request.session.unspent_utxos_for_executor(prepared)?;
    }
    let bound_min_gas_price =
        public_broadcaster_bound_min_gas_price(request.chain_id, min_gas_price);
    let same_token_fee = request.fee_token == request.token;
    let initial_split = public_broadcaster_amount_split_for_tokens_and_protocol(
        request.amount,
        U256::ZERO,
        request.fee_mode,
        same_token_fee,
        RAILGUN_PROTOCOL_FEE_BPS,
    )?;
    let initial_native_top_up = request
        .native_top_up
        .as_ref()
        .map(|_| {
            desktop_native_top_up_plan_from_unshield_fields(
                request.chain_id,
                &chain,
                request.token,
                request.recipient,
                request.unwrap,
                initial_split.receiver_amount,
                Some(request.fee_token),
                U256::ZERO,
                &utxos,
            )
        })
        .transpose()?;
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
            if let Some(native_top_up) = &initial_native_top_up {
                return native_top_up_approximate_shape(
                    &utxos,
                    request.token,
                    request.fee_token,
                    initial_split.receiver_amount,
                    U256::ZERO,
                    native_top_up,
                )
                .map(|shape| shape.with_executor(request.executor.is_some()));
            }
            let selection = unshield_selection_info_with_separate_broadcaster_fee_seed(
                &utxos,
                request.token,
                request.fee_token,
                initial_split.receiver_amount,
                false,
            )
            .map_err(|error| {
                public_broadcaster_build_error(
                    error,
                    U256::ZERO,
                    initial_split.fee_mode,
                    same_token_fee,
                    RAILGUN_PROTOCOL_FEE_BPS,
                )
            })?;
            Ok(
                unshield_approximate_shape(&selection, selection.max_spendable, request.unwrap)
                    .with_executor(request.executor.is_some()),
            )
        },
    )?;
    let initial_fee_estimate = initial_public_broadcaster_fee_estimate(
        "unshield",
        approximate_public_broadcaster_cost(
            broadcaster.clone(),
            chain.gas.gas_limit_buffer,
            request.token,
            request.fee_token,
            request.amount,
            request.fee_mode,
            RAILGUN_PROTOCOL_FEE_BPS,
            min_gas_price,
            seeded_fee_amount,
            request.custom_fee_amount,
            |split| {
                if let Some(native_top_up) = &initial_native_top_up {
                    return native_top_up_approximate_shape(
                        &utxos,
                        request.token,
                        request.fee_token,
                        split.receiver_amount,
                        split.fee_amount,
                        native_top_up,
                    )
                    .map(|shape| shape.with_executor(request.executor.is_some()));
                }
                let selection = unshield_selection_info_with_broadcaster_fee_token(
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
                        RAILGUN_PROTOCOL_FEE_BPS,
                    )
                })?;
                Ok(
                    unshield_approximate_shape(&selection, selection.max_spendable, request.unwrap)
                        .with_executor(request.executor.is_some()),
                )
            },
        ),
        same_token_fee && request.custom_fee_amount.is_none(),
        &broadcaster,
        min_gas_price,
        bound_min_gas_price,
    )?;

    let signer = request.spend_authorization.signer(
        request.vault_store.as_ref(),
        request.view_session.as_ref(),
        "public broadcaster unshield",
    )?;

    let tx_builder = TransactionBuilder {
        chain_type: 0,
        chain_id: request.chain_id,
        railgun_contract: chain.railgun_contract,
    };

    // A custom fee is an exact approved amount; automatic fees stay within the reviewed limit.
    let maximum_fee = request.maximum_private_fee;
    // The fresh estimate includes a buffer. Do not reject an approved fee merely
    // because that buffer grew; the RPC estimate below must still fit the limit.
    let mut fee_amount = starting_public_broadcaster_fee(
        request.custom_fee_amount,
        maximum_fee,
        initial_fee_estimate.as_ref(),
    )?;
    for attempt in 1..=PUBLIC_BROADCASTER_FEE_ATTEMPTS {
        let split = public_broadcaster_amount_split_for_tokens_and_protocol(
            request.amount,
            fee_amount,
            request.fee_mode,
            same_token_fee,
            RAILGUN_PROTOCOL_FEE_BPS,
        )?;
        let native_top_up = request
            .native_top_up
            .as_ref()
            .map(|_| {
                desktop_native_top_up_plan_from_unshield_fields(
                    request.chain_id,
                    &chain,
                    request.token,
                    request.recipient,
                    request.unwrap,
                    split.receiver_amount,
                    Some(request.fee_token),
                    fee_amount,
                    &utxos,
                )
            })
            .transpose()?;
        update_transaction_generation_stage(
            request.progress_tx.as_ref(),
            TransactionGenerationStage::ProvingTransaction,
        );
        let proof_started = Instant::now();
        let plan = if let Some(mut composite_request) = desktop_composite_unshield_request(
            request.token,
            split.receiver_amount,
            request.recipient,
            request.unwrap,
            request.verify_proof,
            native_top_up.as_ref(),
            executor,
        )? {
            composite_request.broadcaster_fee = Some(BroadcasterFeeOutput {
                recipient: broadcaster.address_data,
                token_address: request.fee_token,
                amount: fee_amount,
            });
            composite_request.min_gas_price = bound_min_gas_price;
            DesktopUnshieldPreparedPlan::Composite(
                tx_builder
                    .build_composite_unshield_plan_with_signer(
                        &request.view_session.scan_keys(),
                        &signer,
                        &forest,
                        &utxos,
                        composite_request,
                        &prover,
                    )
                    .await
                    .map_err(|error| {
                        public_broadcaster_build_error(
                            error,
                            fee_amount,
                            split.fee_mode,
                            same_token_fee,
                            RAILGUN_PROTOCOL_FEE_BPS,
                        )
                    })
                    .wrap_err("build public broadcaster composite unshield proof")?,
            )
        } else {
            let unshield_request = RailgunUnshieldRequest {
                token_address: request.token,
                amount: split.receiver_amount,
                recipient: request.recipient,
                verify_proof: request.verify_proof,
                spend_up_to: false,
                broadcaster_fee: Some(BroadcasterFeeOutput {
                    recipient: broadcaster.address_data,
                    token_address: request.fee_token,
                    amount: fee_amount,
                }),
                min_gas_price: bound_min_gas_price,
            };
            DesktopUnshieldPreparedPlan::Single(
                tx_builder
                    .build_unshield_plan_with_signer(
                        &request.view_session.scan_keys(),
                        &signer,
                        &forest,
                        &utxos,
                        unshield_request,
                        &prover,
                    )
                    .await
                    .map_err(|error| {
                        public_broadcaster_build_error(
                            error,
                            fee_amount,
                            split.fee_mode,
                            same_token_fee,
                            RAILGUN_PROTOCOL_FEE_BPS,
                        )
                    })
                    .wrap_err("build public broadcaster unshield proof")?,
            )
        };
        require_pinned_payload(
            pinned,
            single_payload_inputs(
                plan.chunks()
                    .iter()
                    .map(|chunk| chunk.inputs.iter().map(|input| &input.utxo)),
                request.token,
            )
            .as_deref(),
        )?;
        tracing::info!(target: LOG_TARGET,
            attempt,
            elapsed_ms = proof_started.elapsed().as_millis(),
            transaction_count = plan.transaction_count(),
            input_count = plan.input_count(),
            private_output_count = plan.private_output_count(),
            public_output_count = plan.public_output_count(),
            native_top_up = native_top_up.is_some(),
            broadcaster = %broadcaster.railgun_address,
            fees_id = %broadcaster.fees_id,
            "built public broadcaster unshield proof"
        );
        update_transaction_generation_stage(
            request.progress_tx.as_ref(),
            TransactionGenerationStage::EstimatingBroadcasterFee,
        );
        let gas_started = Instant::now();
        let mut transaction = issue_desktop_executor_plan(
            &request.session,
            request
                .executor
                .as_deref()
                .map(|prepared| (prepared, &client)),
            &request.spend_authorization,
            &plan,
        )
        .await?
        .unwrap_or_else(|| {
            TransactionRequest::default()
                .to(plan.call_to())
                .input(plan.call_data().into())
        });
        if transaction.transaction_type == Some(4) {
            transaction.max_fee_per_gas = Some(public_broadcaster_service_gas_price(min_gas_price));
            transaction.max_priority_fee_per_gas = Some(min_gas_price);
        }
        let executor_issue_elapsed_ms = gas_started.elapsed().as_millis();
        let rpc_started = Instant::now();
        let (gas_limit, computed_fee) = estimate_public_broadcaster_fee_from_rpc_pool(
            &query_rpc_pool,
            request.chain_id,
            transaction.clone(),
            broadcaster.fee,
            min_gas_price,
            chain.gas.gas_limit_buffer,
        )
        .await?;
        let rpc_elapsed_ms = rpc_started.elapsed().as_millis();
        let gas_elapsed_ms = gas_started.elapsed().as_millis();
        tracing::debug!(target: LOG_TARGET,
            attempt,
            available_fee = %fee_amount,
            computed_fee = %computed_fee,
            gas_limit,
            min_gas_price,
            bound_min_gas_price,
            executor_issue_elapsed_ms,
            rpc_elapsed_ms,
            gas_elapsed_ms,
            broadcaster = %broadcaster.railgun_address,
            fees_id = %broadcaster.fees_id,
            "estimated public broadcaster unshield fee"
        );
        if broadcaster_fee_covers(fee_amount, computed_fee) {
            // No more proof or executor signatures are needed after fee convergence.
            drop(signer);
            drop(request.spend_authorization);
            transaction.gas = Some(gas_limit);
            let reported_amounts = public_broadcaster_reported_amounts(
                request.token,
                request.fee_token,
                split,
                RAILGUN_PROTOCOL_FEE_BPS,
                native_top_up.as_ref(),
            );
            tracing::debug!(target: LOG_TARGET,
                attempt,
                fee_amount = %fee_amount,
                computed_fee = %computed_fee,
                gas_limit,
                broadcaster = %broadcaster.railgun_address,
                fees_id = %broadcaster.fees_id,
                "public broadcaster unshield fee stabilized"
            );
            update_transaction_generation_stage(
                request.progress_tx.as_ref(),
                TransactionGenerationStage::GeneratingPoiProofs,
            );
            let pre_transaction_pois = public_broadcaster_pre_transaction_pois(
                plan.chunks(),
                &broadcaster,
                request.session.as_ref(),
                request.chain_id,
                &prover,
                request.verify_proof,
                http,
            )
            .await?;
            let pending_persist_started = Instant::now();
            let pending_contexts = match &plan {
                DesktopUnshieldPreparedPlan::Single(plan) => {
                    persist_pending_unshield_output_poi_contexts(
                        request.session.as_ref(),
                        &plan.chunks,
                        &pre_transaction_pois.pending_pois,
                        &pre_transaction_pois.pending_poi_list_keys,
                        true,
                        !same_token_fee,
                    )
                    .await?
                }
                DesktopUnshieldPreparedPlan::Composite(plan) => {
                    persist_pending_composite_unshield_output_poi_contexts(
                        request.session.as_ref(),
                        &plan.chunks,
                        &plan.private_output_roles,
                        &pre_transaction_pois.pending_pois,
                        &pre_transaction_pois.pending_poi_list_keys,
                    )
                    .await?
                }
            };
            tracing::info!(target: LOG_TARGET,
                chain_id = request.chain_id,
                pending_contexts,
                elapsed_ms = pending_persist_started.elapsed().as_millis(),
                "persisted public broadcaster unshield pending output POI contexts"
            );
            let relay_call_count = match &plan {
                DesktopUnshieldPreparedPlan::Single(_) => usize::from(request.unwrap),
                DesktopUnshieldPreparedPlan::Composite(plan) => plan.shape.relay_call_count,
            };
            let uses_relay_adapt = match &plan {
                DesktopUnshieldPreparedPlan::Single(_) => request.unwrap,
                DesktopUnshieldPreparedPlan::Composite(plan) => plan.shape.uses_relay_adapt,
            };
            return Ok(PreparedPublicBroadcasterPlan {
                transaction: Some(transaction),
                transaction_count: plan.transaction_count(),
                input_count: plan.input_count(),
                private_output_count: plan.private_output_count(),
                public_output_count: plan.public_output_count(),
                relay_call_count,
                uses_relay_adapt,
                plan,
                pre_transaction_pois_per_txid_leaf_per_list: pre_transaction_pois.request_pois,
                broadcaster,
                action_token: request.token,
                fee_token: request.fee_token,
                entered_amount: split.entered_amount,
                receiver_amount: split.receiver_amount,
                recipient_amount: reported_amounts.recipient_amount,
                total_private_spend: reported_amounts.total_private_spend,
                fee_amount,
                protocol_fee_amount: reported_amounts.protocol_fee_amount,
                protocol_fee_bps: RAILGUN_PROTOCOL_FEE_BPS,
                fee_mode: split.fee_mode,
                gas_limit,
                min_gas_price,
                bound_min_gas_price,
                native_top_up,
            });
        }
        let next_fee = request.custom_fee_amount.map_or_else(
            || bounded_public_broadcaster_fee(computed_fee, maximum_fee),
            |amount| validate_custom_public_broadcaster_fee(amount, computed_fee, maximum_fee),
        )?;
        log_public_broadcaster_fee_prediction_failure(
            "unshield",
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
        tracing::debug!(target: LOG_TARGET,
            attempt,
            previous_fee = %fee_amount,
            computed_fee = %computed_fee,
            next_fee = %next_fee,
            "retrying public broadcaster unshield proof with buffered fee"
        );
        fee_amount = next_fee;
    }

    Err(eyre!(
        "public broadcaster fee did not stabilize after bounded retries"
    ))
}
