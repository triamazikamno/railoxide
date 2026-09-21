use super::*;
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::railgun::{RelayAdapt7702, relayCall};
use broadcaster_core::transact::{
    BroadcasterAuthorization, BroadcasterTransactRequestType, compute_railgun_txid,
    railgun_txid_leaf_hash,
};
use eyre::eyre;

pub(super) async fn prepare_desktop_unshield_public_broadcaster(
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
    let chain = effective_desktop_chain_config(request.chain_id, &request.effective_chain)?;
    let executor =
        validate_desktop_executor_preparation(&request.session, request.executor.as_deref())?;
    if executor.is_some() && request.executor_maximum_private_fee.is_none() {
        return Err(eyre!(
            "review the executor fee limit before proving this operation"
        ));
    }
    if request.unwrap && !is_effective_wrapped_native_token(request.chain_id, request.token, &chain)
    {
        return Err(eyre!("selected token does not support unwrap-to-native"));
    }

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
        &request.fee_rows,
        &request.selection,
        request.executor.is_none() && (request.unwrap || request.native_top_up.is_some()),
        request
            .executor
            .as_ref()
            .map(|prepared| prepared.delivery()),
        request.fee_policy,
        &request.trust_filter,
        request.anchor_cache.as_ref(),
        http,
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
    let seeded_fee_amount =
        initial_public_broadcaster_fee_amount(&broadcaster, min_gas_price, same_token_fee, || {
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
        })?;
    let initial_fee_estimate = match approximate_public_broadcaster_cost(
        broadcaster.clone(),
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
    ) {
        Ok(estimate) => {
            tracing::info!(
                fee_amount = %estimate.fee_amount,
                gas_limit = estimate.gas_limit,
                min_gas_price,
                bound_min_gas_price,
                transaction_count = estimate.transaction_count,
                input_count = estimate.input_count,
                private_output_count = estimate.private_output_count,
                public_output_count = estimate.public_output_count,
                broadcaster = %broadcaster.railgun_address,
                fees_id = %broadcaster.fees_id,
                "using approximate public broadcaster unshield fee for first proof"
            );
            Some(estimate)
        }
        Err(err) => {
            if !same_token_fee || request.custom_fee_amount.is_some() {
                return Err(err).wrap_err("estimate initial public broadcaster unshield fee");
            }
            tracing::warn!(
                ?err,
                broadcaster = %broadcaster.railgun_address,
                fees_id = %broadcaster.fees_id,
                "failed to estimate initial same-token public broadcaster unshield fee; starting at zero"
            );
            None
        }
    };
    let initial_fee_amount = initial_fee_estimate
        .as_ref()
        .map_or(U256::ZERO, |estimate| estimate.fee_amount);

    let signer = request.spend_authorization.signer(
        request.vault_store.as_ref(),
        request.view_session.as_ref(),
        "public broadcaster unshield",
    )?;

    let mode = if request.unwrap {
        UnshieldMode::UnwrapBase
    } else {
        UnshieldMode::Token
    };
    let tx_builder = TransactionBuilder {
        chain_type: 0,
        chain_id: request.chain_id,
        railgun_contract: chain.railgun_contract,
        relay_adapt_contract: chain.relay_adapt_contract,
    };

    let maximum_fee = executor.and(request.executor_maximum_private_fee);
    // The fresh estimate includes a buffer. Do not reject an approved fee merely
    // because that buffer grew; the RPC estimate below must still fit the limit.
    let mut fee_amount = match request.custom_fee_amount {
        Some(amount) => validate_custom_public_broadcaster_fee(amount, U256::ZERO, maximum_fee)?,
        None => maximum_fee.map_or(initial_fee_amount, |maximum| {
            initial_fee_amount.min(maximum)
        }),
    };
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
                mode,
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
        tracing::info!(
            attempt,
            fee_amount = %fee_amount,
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
            request.executor.as_deref(),
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
        tracing::info!(
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
            tracing::info!(
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
            tracing::info!(
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
        tracing::info!(
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

pub(super) async fn prepare_desktop_send_public_broadcaster(
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
        &request.fee_rows,
        &request.selection,
        false,
        None,
        request.fee_policy,
        &request.trust_filter,
        request.anchor_cache.as_ref(),
        http,
    )
    .await?;
    let bound_min_gas_price =
        public_broadcaster_bound_min_gas_price(request.chain_id, min_gas_price);
    let same_token_fee = request.fee_token == request.token;
    update_transaction_generation_stage(
        request.progress_tx.as_ref(),
        TransactionGenerationStage::SelectingPrivateNotes,
    );
    let seeded_fee_amount =
        initial_public_broadcaster_fee_amount(&broadcaster, min_gas_price, same_token_fee, || {
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
        })?;
    let initial_fee_estimate = match approximate_public_broadcaster_cost(
        broadcaster.clone(),
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
    ) {
        Ok(estimate) => {
            tracing::info!(
                fee_amount = %estimate.fee_amount,
                gas_limit = estimate.gas_limit,
                min_gas_price,
                bound_min_gas_price,
                transaction_count = estimate.transaction_count,
                input_count = estimate.input_count,
                private_output_count = estimate.private_output_count,
                public_output_count = estimate.public_output_count,
                broadcaster = %broadcaster.railgun_address,
                fees_id = %broadcaster.fees_id,
                "using approximate public broadcaster send fee for first proof"
            );
            Some(estimate)
        }
        Err(err) => {
            if !same_token_fee || request.custom_fee_amount.is_some() {
                return Err(err).wrap_err("estimate initial public broadcaster send fee");
            }
            tracing::warn!(
                ?err,
                broadcaster = %broadcaster.railgun_address,
                fees_id = %broadcaster.fees_id,
                "failed to estimate initial same-token public broadcaster send fee; starting at zero"
            );
            None
        }
    };
    let initial_fee_amount = initial_fee_estimate
        .as_ref()
        .map_or(U256::ZERO, |estimate| estimate.fee_amount);

    let signer = request.spend_authorization.into_signer(
        request.vault_store.as_ref(),
        request.view_session.as_ref(),
        "public broadcaster send",
    )?;

    let tx_builder = TransactionBuilder {
        chain_type: 0,
        chain_id: request.chain_id,
        railgun_contract: chain.railgun_contract,
        relay_adapt_contract: chain.relay_adapt_contract,
    };

    let mut fee_amount = request
        .custom_fee_amount
        .map_or(Ok(initial_fee_amount), |amount| {
            validate_custom_public_broadcaster_fee(amount, U256::ZERO, None)
        })?;
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
        tracing::info!(
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
        tracing::info!(
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
            tracing::info!(
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
            tracing::info!(
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
            || Ok(buffered_public_broadcaster_fee(computed_fee)),
            |amount| validate_custom_public_broadcaster_fee(amount, computed_fee, None),
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
        tracing::info!(
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

pub(super) async fn estimate_public_broadcaster_fee(
    provider: &(impl Provider + Clone),
    chain_id: u64,
    transaction: TransactionRequest,
    token_fee_per_unit_gas: U256,
    min_gas_price: u128,
    gas_limit_buffer: u64,
) -> Result<(u64, U256)> {
    let tx_req = if transaction.transaction_type == Some(4) {
        transaction.with_chain_id(chain_id)
    } else {
        transaction
            .with_chain_id(chain_id)
            .with_gas_price(min_gas_price)
    };
    let delegated = tx_req.transaction_type == Some(4);
    if delegated {
        // Some RPCs silently ignore authorizations and simulate an empty account.
        // Require the executor's getter to return ABI data through this delegation.
        let nonce_request = tx_req
            .clone()
            .input(RelayAdapt7702::nonceCall {}.abi_encode().into());
        let nonce_result = provider
            .call(nonce_request)
            .pending()
            .await
            .wrap_err("simulate public broadcaster executor delegation")?;
        RelayAdapt7702::nonceCall::abi_decode_returns(&nonce_result)
            .wrap_err("RPC did not simulate public broadcaster executor delegation")?;
    }
    let estimated_gas = provider
        .estimate_gas(tx_req.clone())
        .await
        .wrap_err("estimate public broadcaster gas")?;
    if delegated {
        // eth_call and eth_estimateGas can have different authorization support.
        // Check the estimate before the buffer can conceal an execution shortfall.
        provider
            .call(tx_req.with_gas_limit(estimated_gas))
            .pending()
            .await
            .wrap_err("simulate public broadcaster transaction at estimated gas")?;
    }
    let gas_limit = public_broadcaster_gas_limit_with_buffer(estimated_gas, gas_limit_buffer);
    let service_gas_price = public_broadcaster_service_gas_price(min_gas_price);
    Ok((
        gas_limit,
        broadcaster_fee_amount(token_fee_per_unit_gas, gas_limit, service_gas_price),
    ))
}

pub(super) async fn estimate_public_broadcaster_fee_from_rpc_pool(
    query_rpc_pool: &QueryRpcPool,
    chain_id: u64,
    transaction: TransactionRequest,
    token_fee_per_unit_gas: U256,
    min_gas_price: u128,
    gas_limit_buffer: u64,
) -> Result<(u64, U256)> {
    let mut last_error = None;
    for _ in 0..query_rpc_pool.len() {
        let Some(provider_handle) = query_rpc_pool.random_provider() else {
            break;
        };
        match estimate_public_broadcaster_fee(
            &provider_handle.provider,
            chain_id,
            transaction.clone(),
            token_fee_per_unit_gas,
            min_gas_price,
            gas_limit_buffer,
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(error) => {
                let rpc = http::redact_url_for_display(&provider_handle.url);
                tracing::warn!(%error, %rpc, "estimate public broadcaster gas failed");
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(error);
            }
        }
    }
    if let Some(error) = last_error {
        Err(error).wrap_err("all query RPC public broadcaster gas estimate attempts failed")
    } else {
        Err(eyre!("no healthy query RPC available"))
    }
}

pub(crate) const fn public_broadcaster_gas_limit_with_buffer(
    estimated_gas: u64,
    gas_limit_buffer: u64,
) -> u64 {
    estimated_gas.saturating_add(gas_limit_buffer)
}

pub(crate) fn public_broadcaster_transact_params(
    broadcaster: &PublicBroadcasterCandidate,
    transaction: TransactionRequest,
    min_gas_price: u128,
    pre_transaction_pois_per_txid_leaf_per_list: PreTransactionPoiMap,
) -> Result<BroadcasterRawParamsTransact> {
    // The broadcaster owns the outer sender/nonce and this wire format has no
    // value field. Refuse an unrepresentable request instead of dropping fields.
    if transaction
        .chain_id
        .is_some_and(|chain| chain != broadcaster.chain_id)
        || transaction.from.is_some()
        || transaction.nonce.is_some()
        || transaction.value.is_some_and(|value| value != U256::ZERO)
        || transaction
            .transaction_type
            .is_some_and(|kind| kind != 2 && kind != 4)
    {
        return Err(eyre!(
            "transaction cannot be represented by the selected broadcaster route"
        ));
    }
    let to = transaction
        .to
        .and_then(alloy::primitives::TxKind::into_to)
        .ok_or_else(|| eyre!("broadcaster transaction destination is unavailable"))?;
    let data = transaction
        .input
        .into_input()
        .ok_or_else(|| eyre!("broadcaster transaction calldata is unavailable"))?;
    let authorization = match transaction.authorization_list.as_deref() {
        None => None,
        Some([authorization]) => Some(BroadcasterAuthorization {
            address: authorization.inner().address,
            nonce: U256::from(authorization.inner().nonce),
            chain_id: authorization.inner().chain_id,
            signature: alloy::serde::WithOtherFields::new(authorization.signature()?),
            other: alloy::serde::OtherFields::default(),
        }),
        Some(_) => {
            return Err(eyre!(
                "broadcaster executor delivery requires one delegation authorization"
            ));
        }
    };
    if authorization.is_some() != (transaction.transaction_type == Some(4)) {
        return Err(eyre!(
            "broadcaster transaction type does not match its authorization"
        ));
    }
    if authorization.is_some()
        && (transaction.max_fee_per_gas.is_none() || transaction.max_priority_fee_per_gas.is_none())
    {
        return Err(eyre!(
            "executor broadcaster delivery requires approved gas fee caps"
        ));
    }
    let uses_executor = data.starts_with(&RelayAdapt7702::executeCall::SELECTOR);
    if uses_executor {
        let call = RelayAdapt7702::executeCall::abi_decode(&data)?;
        if call._transactions.is_empty()
            || broadcaster.available_wallets == 0
            || broadcaster.fee_expiration <= SystemTime::now()
        {
            return Err(eyre!(
                "executor broadcaster fee selection is no longer available"
            ));
        }
        let profile = broadcaster
            .relay_adapt_7702
            .and_then(|delegate| {
                settings::ExecutorProfile::accepted(broadcaster.chain_id, delegate)
            })
            .ok_or_else(|| eyre!("broadcaster executor profile is unavailable"))?;
        if !public_broadcaster_protocol::supports_nonce_bearing_executor(
            broadcaster.relay_adapt_7702,
            profile.delegate(),
        ) {
            return Err(eyre!(
                "broadcaster does not advertise the required executor format"
            ));
        }
        let authorization = authorization.as_ref().ok_or_else(|| {
            eyre!("executor broadcaster delivery requires a fresh delegation authorization")
        })?;
        if authorization.chain_id != U256::from(broadcaster.chain_id)
            || authorization.address != profile.delegate()
            || authorization.signed_authorization()?.recover_authority()? != to
        {
            return Err(eyre!(
                "executor authorization does not match the selected delivery"
            ));
        }
        let list_keys = broadcaster.parsed_required_poi_list_keys()?;
        for inner in &call._transactions {
            let txid = compute_railgun_txid(inner, Some(DEFAULT_TXID_VERSION))?;
            let leaf = FixedBytes::from(railgun_txid_leaf_hash(
                txid,
                u64::from(inner.boundParams.treeNumber),
            ));
            if list_keys.iter().any(|key| {
                !pre_transaction_pois_per_txid_leaf_per_list
                    .get(key)
                    .is_some_and(|proofs| proofs.contains_key(&leaf))
            }) {
                return Err(eyre!(
                    "executor request is missing a required private transaction POI"
                ));
            }
        }
    } else if authorization.is_some() || data.starts_with(&RelayAdapt7702::multicallCall::SELECTOR)
    {
        return Err(eyre!(
            "broadcaster executor delivery requires the current execute wrapper"
        ));
    }
    // SDK broadcasters require the route flag even for direct transact requests.
    let mut other: alloy::serde::OtherFields = [
        ("minVersion", serde_json::Value::from("8.0.0")),
        ("maxVersion", serde_json::Value::from("8.999.0")),
        // The shared request type has no devLog field. SDK broadcasters use it
        // to return the original error instead of "Unknown Broadcaster error."
        ("devLog", true.into()),
        (
            "useRelayAdapt",
            (uses_executor || data.starts_with(&relayCall::SELECTOR)).into(),
        ),
    ]
    .into_iter()
    .collect();
    if authorization.is_some() {
        let gas_limit = transaction.gas.ok_or_else(|| {
            eyre!("executor broadcaster delivery requires an estimated gas limit")
        })?;
        // The shared wrapper has no gas-limit field; SDK TX7702 expects decimal text.
        other.insert("gasLimit".to_owned(), gas_limit.to_string().into());
    }
    Ok(BroadcasterRawParamsTransact {
        chain_type: 0,
        chain_id: broadcaster.chain_id,
        transact_type: authorization
            .as_ref()
            .map(|_| BroadcasterTransactRequestType::Tx7702),
        min_gas_price: Some(U256::from(min_gas_price)),
        max_fee_per_gas: transaction.max_fee_per_gas.map(U256::from),
        max_priority_fee_per_gas: transaction.max_priority_fee_per_gas.map(U256::from),
        authorization,
        fees_id: Some(broadcaster.fees_id.clone()),
        to,
        data,
        broadcaster_viewing_key: FixedBytes::from(broadcaster.viewing_public_key),
        txid_version: Some(DEFAULT_TXID_VERSION.to_string()),
        pre_transaction_pois_per_txid_leaf_per_list,
        other,
    })
}

pub(super) async fn publish_public_broadcaster_payload(
    waku: &WakuClient,
    pubsub_path: &str,
    transact_topic: &str,
    payload: &[u8],
    attempt: usize,
) -> Result<()> {
    tracing::info!(
        pubsub_path = %pubsub_path,
        transact_topic = %transact_topic,
        payload_len = payload.len(),
        attempt,
        "publishing public broadcaster transact request"
    );
    let publish_started = Instant::now();
    waku.publish(transact_topic, payload)
        .await
        .wrap_err("publish public broadcaster transact request")?;
    tracing::info!(
        pubsub_path = %pubsub_path,
        transact_topic = %transact_topic,
        elapsed_ms = publish_started.elapsed().as_millis(),
        attempt,
        "published public broadcaster transact request"
    );
    Ok(())
}

pub(crate) async fn public_broadcaster_republish_loop<F, Fut>(
    mut stop_rx: oneshot::Receiver<()>,
    republish_interval: Duration,
    mut publish: F,
) where
    F: FnMut(usize) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send,
{
    let mut attempt = 1usize;
    loop {
        tokio::select! {
            _ = &mut stop_rx => break,
            () = tokio::time::sleep(republish_interval) => {
                attempt = attempt.saturating_add(1);
                if let Err(error) = publish(attempt).await {
                    tracing::warn!(%error, attempt, "republish public broadcaster transact request failed");
                }
            }
        }
    }
}

pub(super) async fn submit_public_broadcaster_plan(
    waku: Arc<WakuClient>,
    transaction: TransactionRequest,
    pre_transaction_pois_per_txid_leaf_per_list: PreTransactionPoiMap,
    broadcaster: PublicBroadcasterCandidate,
    action_token: Address,
    fee_token: Address,
    entered_amount: U256,
    receiver_amount: U256,
    recipient_amount: U256,
    total_private_spend: U256,
    fee_amount: U256,
    protocol_fee_amount: U256,
    protocol_fee_bps: U256,
    fee_mode: FeeHandlingMode,
    gas_limit: u64,
    min_gas_price: u128,
    bound_min_gas_price: u128,
    transaction_count: usize,
    input_count: usize,
    private_output_count: usize,
    public_output_count: usize,
    relay_call_count: usize,
    uses_relay_adapt: bool,
    native_top_up: Option<DesktopNativeTopUpPlan>,
    progress_tx: Option<TransactionGenerationProgressSender>,
    timeout: Duration,
    republish_interval: Duration,
) -> Result<PublicBroadcasterSubmissionResult> {
    tracing::info!(
        chain_id = broadcaster.chain_id,
        broadcaster = %broadcaster.railgun_address,
        broadcaster_identifier = ?broadcaster.identifier.as_deref(),
        fees_id = %broadcaster.fees_id,
        token = ?broadcaster.token,
        fee_amount = %fee_amount,
        gas_limit,
        min_gas_price,
        bound_min_gas_price,
        data_len = transaction.input.input().map_or(0, |input| input.len()),
        "preparing public broadcaster transact request"
    );
    let result = submit_public_broadcaster_transaction(
        waku,
        transaction,
        pre_transaction_pois_per_txid_leaf_per_list,
        &broadcaster,
        bound_min_gas_price,
        progress_tx,
        timeout,
        republish_interval,
    )
    .await?;

    Ok(PublicBroadcasterSubmissionResult {
        broadcaster,
        action_token,
        fee_token,
        entered_amount,
        receiver_amount,
        recipient_amount,
        total_private_spend,
        fee_amount,
        protocol_fee_amount,
        protocol_fee_bps,
        fee_mode,
        gas_limit,
        min_gas_price,
        transaction_count,
        input_count,
        private_output_count,
        public_output_count,
        relay_call_count,
        uses_relay_adapt,
        result,
        native_top_up,
    })
}

/// Publish an admitted complete transaction and its POIs through the existing
/// encrypted request/response transport. Dropping the future stops local retries.
pub(super) async fn submit_public_broadcaster_transaction(
    waku: Arc<WakuClient>,
    transaction: TransactionRequest,
    pre_transaction_pois_per_txid_leaf_per_list: PreTransactionPoiMap,
    broadcaster: &PublicBroadcasterCandidate,
    bound_min_gas_price: u128,
    progress_tx: Option<TransactionGenerationProgressSender>,
    timeout: Duration,
    republish_interval: Duration,
) -> Result<PublicBroadcasterResultKind> {
    let transact_topic = transact_topic(broadcaster.chain_id);
    let response_topic = transact_response_topic(broadcaster.chain_id);
    update_transaction_generation_stage(
        progress_tx.as_ref(),
        TransactionGenerationStage::PublishingToBroadcaster,
    );
    let params = public_broadcaster_transact_params(
        broadcaster,
        transaction,
        bound_min_gas_price,
        pre_transaction_pois_per_txid_leaf_per_list,
    )?;
    let encrypt_started = Instant::now();
    let encrypted = EncryptedTransactRequest::encrypt(broadcaster.viewing_public_key, &params)
        .wrap_err("encrypt public broadcaster transact request")?;
    let payload = encrypted
        .to_transact_payload()
        .wrap_err("serialize public broadcaster transact request")?;
    tracing::info!(
        chain_id = broadcaster.chain_id,
        broadcaster = %broadcaster.railgun_address,
        fees_id = %broadcaster.fees_id,
        payload_len = payload.len(),
        elapsed_ms = encrypt_started.elapsed().as_millis(),
        "built public broadcaster encrypted Waku payload"
    );
    let pubsub_path = waku.pubsub_path().to_string();
    tracing::info!(
        pubsub_path = %pubsub_path,
        response_topic = %response_topic,
        "subscribing to public broadcaster response topic"
    );
    let subscribe_started = Instant::now();
    let mut response_rx = waku
        .subscribe(vec![response_topic.clone()])
        .await
        .wrap_err("subscribe to public broadcaster response topic")?;
    tracing::info!(
        response_topic = %response_topic,
        elapsed_ms = subscribe_started.elapsed().as_millis(),
        "subscribed to public broadcaster response topic"
    );
    publish_public_broadcaster_payload(&waku, &pubsub_path, &transact_topic, &payload, 1)
        .await
        .wrap_err("publish initial public broadcaster transact request")?;
    update_transaction_generation_stage(
        progress_tx.as_ref(),
        TransactionGenerationStage::WaitingForBroadcasterResponse,
    );

    let (republish_stop_tx, republish_stop_rx) = oneshot::channel();
    let republish_waku = Arc::clone(&waku);
    let republish_pubsub_path = pubsub_path.clone();
    let republish_transact_topic = transact_topic.clone();
    let republish_payload = payload.clone();
    let republish_handle = tokio::spawn(public_broadcaster_republish_loop(
        republish_stop_rx,
        republish_interval,
        move |attempt| {
            let waku = Arc::clone(&republish_waku);
            let pubsub_path = republish_pubsub_path.clone();
            let transact_topic = republish_transact_topic.clone();
            let payload = republish_payload.clone();
            async move {
                publish_public_broadcaster_payload(
                    &waku,
                    &pubsub_path,
                    &transact_topic,
                    &payload,
                    attempt,
                )
                .await
            }
        },
    ));

    let sleep = tokio::time::sleep(timeout);
    tokio::pin!(sleep);
    let result = loop {
        tokio::select! {
            () = &mut sleep => {
                tracing::warn!(
                    chain_id = broadcaster.chain_id,
                    broadcaster = %broadcaster.railgun_address,
                    fees_id = %broadcaster.fees_id,
                    response_topic = %response_topic,
                    timeout_ms = timeout.as_millis(),
                    "timed out waiting for public broadcaster response"
                );
                break PublicBroadcasterResultKind::TimedOut;
            },
            msg = response_rx.recv() => {
                let Some(msg) = msg else {
                    tracing::warn!(response_topic = %response_topic, "public broadcaster response channel closed");
                    break PublicBroadcasterResultKind::TimedOut;
                };
                tracing::info!(
                    content_topic = %msg.content_topic,
                    payload_len = msg.payload.len(),
                    "received public broadcaster response candidate"
                );
                match decode_public_broadcaster_response(&encrypted.shared_key, &msg.payload) {
                    Ok(Some(result)) => {
                        // Detailed errors can contain the broadcaster's RPC data.
                        // Show them in progress without copying them into wallet logs.
                        tracing::info!(
                            submitted = matches!(result, PublicBroadcasterResultKind::Submitted { .. }),
                            "decrypted public broadcaster response"
                        );
                        break result;
                    }
                    Ok(None) => tracing::debug!("public broadcaster response was not decryptable with request key"),
                    Err(error) => tracing::debug!(%error, "ignoring undecryptable public broadcaster response"),
                }
            }
        }
    };
    let _ = republish_stop_tx.send(());
    republish_handle.abort();

    Ok(result)
}

#[cfg(test)]
mod gas_estimate_tests {
    use super::*;
    use alloy::eips::eip7702::Authorization;
    use alloy::signers::{SignerSync, local::PrivateKeySigner};
    use serde_json::json;

    #[tokio::test]
    async fn broadcaster_gas_requires_delegated_execution_and_preserves_legacy_estimates() {
        #[derive(Clone, Copy, Debug)]
        enum Simulation {
            IgnoresDelegation,
            UnderestimatesExecution,
            Executes,
            Legacy,
        }

        let signer = PrivateKeySigner::from_bytes(&FixedBytes::repeat_byte(7)).unwrap();
        let authorization = Authorization {
            chain_id: U256::ONE,
            address: Address::repeat_byte(0x42),
            nonce: 0,
        };
        let signature = signer
            .sign_hash_sync(&authorization.signature_hash())
            .unwrap();
        let authorization = authorization.into_signed(signature);
        let call_data = Bytes::from_static(&[1, 2, 3]);
        for simulation in [
            Simulation::IgnoresDelegation,
            Simulation::UnderestimatesExecution,
            Simulation::Executes,
            Simulation::Legacy,
        ] {
            let mut transaction = TransactionRequest::default()
                .to(signer.address())
                .input(call_data.clone().into());
            let delegated = !matches!(simulation, Simulation::Legacy);
            if delegated {
                transaction.transaction_type = Some(4);
                transaction.authorization_list = Some(vec![authorization.clone()]);
                transaction.max_fee_per_gas = Some(125);
                transaction.max_priority_fee_per_gas = Some(100);
            }
            let expected_authorization =
                serde_json::to_value(&transaction.authorization_list).unwrap();
            let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
                Arc::new(move |request| {
                    assert_eq!(request["params"][1], "pending");
                    let tx = &request["params"][0];
                    assert_eq!(tx["authorizationList"], expected_authorization);
                    let result = match request["method"].as_str().unwrap() {
                        "eth_estimateGas" => json!(if matches!(simulation, Simulation::Executes) {
                            "0x1bc22a" // Full execution: 1,819,178 gas.
                        } else {
                            "0x1c092" // Empty-account execution: 114,834 gas.
                        }),
                        "eth_call" => {
                            assert!(delegated, "legacy estimates need no delegation checks");
                            let tx: TransactionRequest =
                                serde_json::from_value(tx.clone()).unwrap();
                            if tx.input.input().unwrap().as_ref()
                                == RelayAdapt7702::nonceCall::SELECTOR
                            {
                                if matches!(simulation, Simulation::IgnoresDelegation) {
                                    json!("0x")
                                } else {
                                    json!(Bytes::from(U256::ZERO.to_be_bytes::<32>().to_vec()))
                                }
                            } else {
                                assert_eq!(tx.input.input().unwrap().as_ref(), [1, 2, 3]);
                                if tx.gas.unwrap() < 1_768_554 {
                                    return json!({"jsonrpc": "2.0", "id": request["id"],
                                        "error": {"code": -32000, "message": "out of gas"}});
                                }
                                json!("0x")
                            }
                        }
                        method => panic!("unexpected RPC method: {method}"),
                    };
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                }),
                Arc::default(),
                Arc::default(),
            )
            .await;
            let pool = QueryRpcPool::with_http_client(
                vec![endpoint],
                Duration::from_secs(30),
                HttpContext::direct_for_tests().rpc_client,
            );
            let result = estimate_public_broadcaster_fee_from_rpc_pool(
                &pool,
                1,
                transaction,
                U256::from(1_000_000_000u64),
                100,
                100_000,
            )
            .await;
            server.abort();
            match simulation {
                Simulation::IgnoresDelegation | Simulation::UnderestimatesExecution => {
                    assert!(
                        result.is_err(),
                        "accepted an invalid estimate: {simulation:?}"
                    );
                    assert!(pool.available_providers().is_empty());
                }
                Simulation::Executes | Simulation::Legacy => {
                    let expected = if delegated { 1_919_178 } else { 214_834 };
                    let (gas, fee) = result.unwrap();
                    assert_eq!(gas, expected);
                    assert_eq!(
                        fee,
                        broadcaster_fee_amount(U256::from(1_000_000_000u64), gas, 125)
                    );
                }
            }
        }
    }
}
