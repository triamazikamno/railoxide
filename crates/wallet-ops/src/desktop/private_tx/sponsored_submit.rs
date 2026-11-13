use super::*;

fn validate_sponsorship_plan_output(
    plan: &MixedPrivateActionPlan,
    authorization: &SponsoredAuthorization,
    expected_wrapped_native_output: Option<(usize, U256)>,
    expected_actions: Option<&CompositeRelayActions>,
) -> std::result::Result<(), SponsorshipError> {
    if let Some((expected_unshield_index, expected_wrapped_native_amount)) =
        expected_wrapped_native_output
    {
        let actual_wrapped_native_amount = sponsored_wrapped_native_output_total(
            &plan.public_outputs,
            expected_unshield_index,
            authorization.wrapped_native_token,
        )?;
        if actual_wrapped_native_amount != expected_wrapped_native_amount {
            return Err(SponsorshipError::SponsoredPlanShapeChangeRequired);
        }
    }
    match expected_actions {
        Some(expected_actions) => {
            let expected_action_data = expected_actions
                .action_data(authorization.relay_adapt_contract, FixedBytes::<31>::ZERO)
                .map_err(|_| SponsorshipError::SponsoredPlanShapeChangeRequired)?;
            let Some(action_data) = plan.action_data.as_ref() else {
                return Err(SponsorshipError::SponsoredPlanShapeChangeRequired);
            };
            if plan.call.to != authorization.relay_adapt_contract
                || !action_data.requireSuccess
                || action_data.minGasLimit != expected_action_data.minGasLimit
                || action_data.calls.len() != expected_action_data.calls.len()
                || action_data
                    .calls
                    .iter()
                    .zip(&expected_action_data.calls)
                    .any(|(actual, expected)| {
                        actual.to != expected.to
                            || actual.value != expected.value
                            || actual.data != expected.data
                    })
            {
                return Err(SponsorshipError::SponsoredPlanShapeChangeRequired);
            }
        }
        None if plan.action_data.is_some() || plan.shape.uses_relay_adapt => {
            return Err(SponsorshipError::SponsoredPlanShapeChangeRequired);
        }
        None => {}
    }
    Ok(())
}

pub(super) fn sponsored_wrapped_native_output_total(
    public_outputs: &[MixedPublicPlannedOutput],
    expected_unshield_index: usize,
    wrapped_native: Address,
) -> std::result::Result<U256, SponsorshipError> {
    let mut found_output = false;
    let mut total = U256::ZERO;
    for output in public_outputs.iter().filter(|output| {
        output.role == CompositeUnshieldLegRole::SponsoredWrappedNative
            || output.unshield_index == expected_unshield_index
    }) {
        if output.role != CompositeUnshieldLegRole::SponsoredWrappedNative
            || output.unshield_index != expected_unshield_index
            || output.token_address != wrapped_native
            || output.recipient != CompositeUnshieldRecipient::RelayAdapt
        {
            return Err(SponsorshipError::SponsoredPlanShapeChangeRequired);
        }
        found_output = true;
        total = total
            .checked_add(output.amount)
            .ok_or(SponsorshipError::ArithmeticOverflow)?;
    }
    if !found_output {
        return Err(SponsorshipError::SponsoredPlanShapeChangeRequired);
    }
    Ok(total)
}

pub(super) fn optional_sponsored_wrapped_native_leg_amount(
    request: &MixedPrivateActionRequest,
) -> std::result::Result<Option<(usize, U256)>, SponsorshipError> {
    let mut legs = request
        .public_unshields
        .iter()
        .enumerate()
        .filter(|(_, leg)| leg.role == CompositeUnshieldLegRole::SponsoredWrappedNative);
    let Some((index, leg)) = legs.next() else {
        return Ok(None);
    };
    if legs.next().is_some() || leg.recipient != CompositeUnshieldRecipient::RelayAdapt {
        return Err(SponsorshipError::SponsoredPlanShapeChangeRequired);
    }
    Ok(Some((index, leg.amount)))
}

/// Payment to sign when the first plan needs no rebuild. The request and calldata depend only on
/// `builder_payment`, so when it is unchanged a higher exact gas limit is signed with gas and
/// funding fields recomputed for that limit.
pub(super) fn sponsored_payment_without_rebuild(
    provisional: &SponsorshipPayment,
    required: &SponsorshipPayment,
) -> SponsorshipPayment {
    if required.outer_gas_limit > provisional.outer_gas_limit
        && required.builder_payment == provisional.builder_payment
    {
        *required
    } else {
        *provisional
    }
}

pub(super) fn sponsored_rebuild_error(error: BuildError) -> Report {
    match error {
        BuildError::PinnedInputUnavailable { .. }
        | BuildError::PinnedInputsInsufficient(_)
        | BuildError::PinnedInputsChanged { .. }
        | BuildError::CompositePlanShapeChanged { .. } => {
            SponsorshipError::SponsoredPlanShapeChangeRequired.into()
        }
        error => error.into(),
    }
}

/// The Sensitive route of one sponsored self-broadcast, isolated over `client`, with a pool over
/// it. Preparation, submission and observation all read through this one route.
fn sponsored_sensitive_route(
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<(ClassRoute, Arc<QueryRpcPool>)> {
    if railgun_ui::is_testnet_chain(chain_id) {
        return Err(eyre!(
            "Sponsored self-broadcast is unavailable on testnets."
        ));
    }
    let route = effective_desktop_chain_config(chain_id, effective_chain)?
        .rpc_route
        .for_class(RpcTrafficClass::Sensitive)?
        .isolated(client.clone());
    let query_rpc_pool = query_rpc_pool_for_route(&route, http);
    Ok((route, query_rpc_pool))
}

#[allow(clippy::too_many_arguments)]
async fn prepare_desktop_sponsored_calldata(
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    view_session: &vault::DesktopViewSession,
    session: &WalletSession,
    vault_store: &vault::DesktopVaultStore,
    spend_authorization: DesktopPrivateSpendAuthorization,
    public_account_uuid: &str,
    intent: SponsoredPrivateIntent,
    native_top_up_requested: bool,
    verify_proof: bool,
    gas_fee: SelfBroadcastGasFeeSelection,
    incentive: SponsoredIncentive,
    authorization_limit: &SponsoredAuthorizationLimit,
    progress_tx: Option<&TransactionGenerationProgressSender>,
    http: &HttpContext,
    sensitive_route: &ClassRoute,
    query_rpc_pool: &QueryRpcPool,
) -> Result<PreparedSponsoredCall> {
    if session.chain_id != chain_id {
        return Err(eyre!(
            "selected wallet session is for chain {}, not {}",
            session.chain_id,
            chain_id
        ));
    }
    if effective_chain.chain_id != chain_id {
        return Err(eyre!(
            "effective chain config is for chain {}, not {chain_id}",
            effective_chain.chain_id
        ));
    }
    let chain = effective_desktop_chain_config(chain_id, effective_chain)?;
    let wrapped_native = chain
        .wrapped_native_token
        .ok_or(SponsorshipError::MissingWrappedNativeToken)?;
    let payer = effective_chain
        .require_railgun()?
        .coinbase_payer
        .ok_or(SponsorshipError::MissingCoinbasePayer)?;
    let relay_adapt = chain
        .relay_adapt_contract
        .ok_or(SponsorshipError::MissingRelayAdapt)?;
    let signer_address = self_broadcast_gas_payer(vault_store, view_session, public_account_uuid)?;

    let provider_handle =
        sponsored_code_preflight_from_rpc_pool(query_rpc_pool, payer, signer_address).await?;
    let utxos = session.unspent_utxos();
    let native_top_up = if native_top_up_requested {
        let SponsoredPrivateIntent::Unshield {
            token,
            amount,
            recipient,
            unwrap,
        } = intent
        else {
            return Err(eyre!(
                "native top-up is available only for sponsored Unshield"
            ));
        };
        Some(desktop_native_top_up_plan_from_unshield_fields(
            chain_id,
            &chain,
            token,
            recipient,
            unwrap,
            amount,
            None,
            U256::ZERO,
            &utxos,
        )?)
    } else {
        None
    };
    let poi_spendable_wrapped_native = poi_spendable_token_balance(&utxos, wrapped_native);
    // Admission requires wrapped native for the approved ceiling, so it may refuse sends whose
    // exact cost would fit.
    validate_sponsored_admission(SponsoredAdmission {
        action: intent.action(),
        delivery: PrivateDeliveryMode::SelfBroadcast,
        has_relays: !effective_chain
            .require_railgun()?
            .sponsored_bundle_relays
            .is_empty(),
        wrapped_native_token: Some(wrapped_native),
        coinbase_payer: Some(payer),
        relay_adapt_contract: chain.relay_adapt_contract,
        payer_verified: true,
        signer_eligible: true,
        poi_spendable_wrapped_native,
        required_wrapped_native: authorization_limit.max_total_wrapped_native_spend,
    })?;

    let quote = self_broadcast_gas_fee_quote_from_route(http, sensitive_route)
        .await
        .wrap_err("fetch sponsored self-broadcast gas price")?;
    let resolved_fee = resolve_self_broadcast_gas_fee(gas_fee, quote)?;
    let action_fingerprint = match intent {
        SponsoredPrivateIntent::Send {
            token,
            amount,
            recipient,
        } => sponsored_send_action_fingerprint(chain_id, token, amount, &recipient),
        SponsoredPrivateIntent::Unshield {
            token,
            amount,
            recipient,
            unwrap,
        } => sponsored_unshield_action_fingerprint(
            chain_id,
            token,
            amount,
            recipient,
            unwrap,
            native_top_up.as_ref(),
        ),
    };
    if authorization_limit.action_fingerprint != action_fingerprint
        || authorization_limit.action != intent.action()
        || authorization_limit.wrapped_native_token != wrapped_native
        || authorization_limit.coinbase_payer != payer
        || authorization_limit.relay_adapt_contract != relay_adapt
        || authorization_limit.max_fee_per_gas != resolved_fee.max_fee_per_gas
        || authorization_limit.max_priority_fee_per_gas != resolved_fee.max_priority_fee_per_gas
        || authorization_limit.incentive != incentive
        || authorization_limit.signer != signer_address
        || authorization_limit.delivery != PrivateDeliveryMode::SelfBroadcast
    {
        return Err(SponsorshipError::AuthorizationMismatch.into());
    }
    let provisional_payment = sponsored_provisional_payment_for_intent(
        chain_id,
        &chain,
        &utxos,
        wrapped_native,
        payer,
        relay_adapt,
        signer_address,
        intent,
        native_top_up.as_ref(),
        resolved_fee.max_fee_per_gas,
        resolved_fee.max_priority_fee_per_gas,
        authorization_limit.signer_native_balance_snapshot,
        incentive,
        GasEstimateMode::Expected,
    )?;
    let provisional_authorization = sponsored_authorization(
        intent.action(),
        wrapped_native,
        payer,
        relay_adapt,
        provisional_payment,
        resolved_fee.max_fee_per_gas,
        resolved_fee.max_priority_fee_per_gas,
        signer_address,
    );

    let artifact_source = artifact_source(http, session.db.as_ref())?;
    let prover = ProverService::new_with_db(&artifact_source, &session.db);
    let chain_handle = session
        .sync_manager
        .chain_handle(&session.chain_key)
        .await
        .ok_or_else(|| eyre!("chain handle not found for chain {chain_id}"))?;
    let mut forest = chain_handle.forest.read().await.clone();
    forest.compute_roots();
    let signer =
        spend_authorization.into_signer(vault_store, view_session, "sponsored private action")?;
    let tx_builder = TransactionBuilder {
        chain_type: 0,
        chain_id,
        railgun_contract: chain.railgun_contract,
    };
    update_transaction_generation_stage(
        progress_tx,
        TransactionGenerationStage::ProvingTransaction,
    );
    let first_request = intent.mixed_request(
        &provisional_authorization,
        native_top_up.as_ref(),
        verify_proof,
        None,
    )?;
    let first_expected_wrapped_native_output =
        optional_sponsored_wrapped_native_leg_amount(&first_request)?;
    let first_expected_actions = first_request.relay_actions.clone();
    let first_plan = tx_builder
        .build_mixed_private_action_plan_with_signer(
            &view_session.scan_keys(),
            &signer,
            &forest,
            &utxos,
            first_request,
            &prover,
        )
        .await
        .map_err(Report::new)?;
    validate_sponsorship_plan_output(
        &first_plan,
        &provisional_authorization,
        first_expected_wrapped_native_output,
        first_expected_actions.as_ref(),
    )?;
    let first_estimated_gas = sponsored_exact_gas_estimate(
        &provider_handle,
        first_plan.call.to,
        first_plan.call.data.clone(),
    )
    .await?;
    let first_gas_limit =
        sponsored_gas_limit_with_buffer(first_estimated_gas, chain.gas.gas_limit_buffer)?;
    let first_required_payment = sponsorship_payment(
        first_gas_limit,
        resolved_fee.max_fee_per_gas,
        authorization_limit.signer_native_balance_snapshot,
        incentive,
    )?;

    let (plan, authorization) =
        if sponsored_payment_requires_rebuild(provisional_payment, first_required_payment) {
            let required_authorization = sponsored_authorization(
                intent.action(),
                wrapped_native,
                payer,
                relay_adapt,
                first_required_payment,
                resolved_fee.max_fee_per_gas,
                resolved_fee.max_priority_fee_per_gas,
                signer_address,
            );
            let rebuild = MixedPrivateActionRebuildConstraint {
                selected_inputs: first_plan.selected_inputs.clone(),
                expected_shape: first_plan.shape,
            };
            let rebuilt_request = intent.mixed_request(
                &required_authorization,
                native_top_up.as_ref(),
                verify_proof,
                Some(rebuild),
            )?;
            let rebuilt_expected_wrapped_native_output =
                optional_sponsored_wrapped_native_leg_amount(&rebuilt_request)?;
            let rebuilt_expected_actions = rebuilt_request.relay_actions.clone();
            let rebuilt_plan = tx_builder
                .build_mixed_private_action_plan_with_signer(
                    &view_session.scan_keys(),
                    &signer,
                    &forest,
                    &utxos,
                    rebuilt_request,
                    &prover,
                )
                .await
                .map_err(sponsored_rebuild_error)?;
            validate_sponsorship_plan_output(
                &rebuilt_plan,
                &required_authorization,
                rebuilt_expected_wrapped_native_output,
                rebuilt_expected_actions.as_ref(),
            )?;
            let rebuilt_estimated_gas = sponsored_exact_gas_estimate(
                &provider_handle,
                rebuilt_plan.call.to,
                rebuilt_plan.call.data.clone(),
            )
            .await?;
            let rebuilt_gas_limit =
                sponsored_gas_limit_with_buffer(rebuilt_estimated_gas, chain.gas.gas_limit_buffer)?;
            let final_required_payment = sponsorship_payment(
                rebuilt_gas_limit,
                resolved_fee.max_fee_per_gas,
                authorization_limit.signer_native_balance_snapshot,
                incentive,
            )?;
            validate_final_sponsorship_payment(first_required_payment, final_required_payment)?;
            (rebuilt_plan, required_authorization)
        } else {
            let payment =
                sponsored_payment_without_rebuild(&provisional_payment, &first_required_payment);
            let authorization = sponsored_authorization(
                intent.action(),
                wrapped_native,
                payer,
                relay_adapt,
                payment,
                resolved_fee.max_fee_per_gas,
                resolved_fee.max_priority_fee_per_gas,
                signer_address,
            );
            (first_plan, authorization)
        };
    drop(signer);

    let public_wrapped_native_spend = plan
        .public_outputs
        .iter()
        .filter(|output| output.token_address == wrapped_native)
        .try_fold(U256::ZERO, |total, output| {
            total
                .checked_add(output.amount)
                .ok_or(SponsorshipError::ArithmeticOverflow)
        })?;
    let total_wrapped_native_spend =
        sponsored_total_wrapped_native_spend(intent, public_wrapped_native_spend, wrapped_native)?;
    let action_fingerprint = match intent {
        SponsoredPrivateIntent::Send {
            token,
            amount,
            recipient,
        } => sponsored_send_action_fingerprint(chain_id, token, amount, &recipient),
        SponsoredPrivateIntent::Unshield {
            token,
            amount,
            recipient,
            unwrap,
        } => sponsored_unshield_action_fingerprint(
            chain_id,
            token,
            amount,
            recipient,
            unwrap,
            native_top_up.as_ref(),
        ),
    };
    validate_sponsored_authorization_limit(
        *authorization_limit,
        action_fingerprint,
        authorization,
        total_wrapped_native_spend,
    )?;

    update_transaction_generation_stage(
        progress_tx,
        TransactionGenerationStage::GeneratingPoiProofs,
    );
    let (pending_poi_list_keys, pending_pois) = active_list_pre_transaction_pois(
        &plan.chunks,
        session,
        chain_id,
        &prover,
        verify_proof,
        http,
        "generate sponsored private pending output pre-transaction POI",
    )
    .await?;
    persist_pending_mixed_output_poi_contexts(
        session,
        &plan.chunks,
        &plan.private_outputs,
        &pending_pois,
        &pending_poi_list_keys,
    )
    .await?;
    Ok(PreparedSponsoredCall {
        chain_id,
        action: intent.action(),
        authorization,
        transaction_count: plan.shape.transaction_count,
        input_count: plan.shape.input_count,
        private_output_count: plan.shape.private_output_count,
        public_output_count: plan.shape.public_output_count,
        relay_call_count: plan.shape.relay_call_count,
        uses_relay_adapt: plan.shape.uses_relay_adapt,
        selected_inputs: plan.selected_inputs,
        native_top_up,
        total_wrapped_native_spend,
        to: plan.call.to,
        data: hex::encode_prefixed(plan.call.data),
    })
}

async fn prepare_desktop_sponsored_send_calldata(
    request: DesktopSponsoredSendCalldataRequest,
    http: &HttpContext,
    sensitive_route: &ClassRoute,
    query_rpc_pool: &QueryRpcPool,
) -> Result<PreparedSponsoredCall> {
    let recipient = parse_railgun_recipient(request.recipient.trim())?;
    prepare_desktop_sponsored_calldata(
        request.chain_id,
        &request.effective_chain,
        request.view_session.as_ref(),
        request.session.as_ref(),
        request.vault_store.as_ref(),
        request.spend_authorization,
        &request.public_account_uuid,
        SponsoredPrivateIntent::Send {
            token: request.token,
            amount: request.amount,
            recipient,
        },
        false,
        request.verify_proof,
        request.gas_fee,
        request.incentive,
        &request.authorization_limit,
        request.progress_tx.as_ref(),
        http,
        sensitive_route,
        query_rpc_pool,
    )
    .await
}

async fn prepare_desktop_sponsored_unshield_calldata(
    request: DesktopSponsoredUnshieldCalldataRequest,
    http: &HttpContext,
    sensitive_route: &ClassRoute,
    query_rpc_pool: &QueryRpcPool,
) -> Result<PreparedSponsoredCall> {
    let chain = effective_desktop_chain_config(request.chain_id, &request.effective_chain)?;
    if request.unwrap && !is_effective_wrapped_native_token(request.chain_id, request.token, &chain)
    {
        return Err(eyre!("selected token does not support unwrap-to-native"));
    }
    let amount = unshield_receiver_amount_for_fee_mode(request.amount, request.fee_mode)?;
    prepare_desktop_sponsored_calldata(
        request.chain_id,
        &request.effective_chain,
        request.view_session.as_ref(),
        request.session.as_ref(),
        request.vault_store.as_ref(),
        request.spend_authorization,
        &request.public_account_uuid,
        SponsoredPrivateIntent::Unshield {
            token: request.token,
            amount,
            recipient: request.recipient,
            unwrap: request.unwrap,
        },
        request.native_top_up.is_some(),
        request.verify_proof,
        request.gas_fee,
        request.incentive,
        &request.authorization_limit,
        request.progress_tx.as_ref(),
        http,
        sensitive_route,
        query_rpc_pool,
    )
    .await
}

pub async fn submit_prepared_desktop_sponsored_self_broadcast(
    request: DesktopPreparedSponsoredSelfBroadcastRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<SponsoredSelfBroadcastSessionOutcome> {
    let (sensitive_route, query_rpc_pool) =
        sponsored_sensitive_route(request.chain_id, &request.effective_chain, http, client)?;
    submit_prepared_sponsored_self_broadcast(
        request,
        None,
        http,
        client,
        sensitive_route,
        query_rpc_pool,
    )
    .await
}

pub async fn submit_desktop_sponsored_send_self_broadcast(
    request: DesktopSponsoredSendSelfBroadcastRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<DesktopSponsoredSelfBroadcastResult> {
    let (spend_authorization, gas_payer_authorization) = request
        .spend_authorization
        .split_hardware_gas_payment(&request.view_session)?;
    let (sensitive_route, query_rpc_pool) =
        sponsored_sensitive_route(request.chain_id, &request.effective_chain, http, client)?;
    let prepared = prepare_desktop_sponsored_send_calldata(
        DesktopSponsoredSendCalldataRequest {
            chain_id: request.chain_id,
            effective_chain: request.effective_chain.clone(),
            view_session: Arc::clone(&request.view_session),
            session: Arc::clone(&request.session),
            vault_store: Arc::clone(&request.vault_store),
            spend_authorization,
            public_account_uuid: request.public_account_uuid.clone(),
            token: request.token,
            amount: request.amount,
            recipient: request.recipient.clone(),
            verify_proof: request.verify_proof,
            gas_fee: request.gas_fee,
            incentive: request.incentive,
            authorization_limit: request.authorization_limit,
            progress_tx: request.progress_tx.clone(),
        },
        http,
        &sensitive_route,
        &query_rpc_pool,
    )
    .await?;
    let activity = sponsored_activity(
        &request.vault_store,
        &request.view_session,
        &prepared,
        vault::ActivityKind::PrivateSend,
        request.token,
        request.amount,
        vault::ActivityCounterparty::Private(request.recipient.trim().to_owned()),
    )
    .with_gateway_origin(request.gateway_origin);
    let outcome = submit_prepared_sponsored_self_broadcast(
        DesktopPreparedSponsoredSelfBroadcastRequest {
            transaction_tracking: request.transaction_tracking,
            chain_id: request.chain_id,
            effective_chain: request.effective_chain,
            view_session: request.view_session,
            session: request.session,
            vault_store: request.vault_store,
            gas_payer_authorization,
            vault_password: request.vault_password,
            protected_software_seed_session: request.protected_software_seed_session,
            trezor_pin_matrix_provider: request.trezor_pin_matrix_provider,
            public_account_uuid: request.public_account_uuid,
            prepared: prepared.clone(),
            progress_tx: request.progress_tx,
            command_rx: request.command_rx,
            event_tx: request.event_tx,
        },
        Some(&activity),
        http,
        client,
        sensitive_route,
        query_rpc_pool,
    )
    .await?;
    Ok(DesktopSponsoredSelfBroadcastResult { prepared, outcome })
}

/// A sponsored private action's journal entry. Gas is paid by the authorized signer, and the
/// builder payment is the fee.
fn sponsored_activity(
    vault_store: &vault::DesktopVaultStore,
    view_session: &Arc<vault::DesktopViewSession>,
    prepared: &PreparedSponsoredCall,
    kind: vault::ActivityKind,
    token: Address,
    amount: U256,
    counterparty: vault::ActivityCounterparty,
) -> ActivitySubmission {
    ActivitySubmission::private(
        vault_store,
        view_session,
        PrivateActivity {
            chain_id: prepared.chain_id,
            kind,
            token,
            amount,
            counterparty,
            fee: Some(vault::ActivityFee {
                asset: vault::ActivityAsset::Token(prepared.authorization.wrapped_native_token),
                amount: prepared.authorization.builder_payment,
            }),
            gas_payer: Some(prepared.authorization.signer),
            inputs: prepared
                .selected_inputs
                .iter()
                .map(|input| vault::ActivityInput {
                    tree: input.tree,
                    position: input.position,
                })
                .collect(),
        },
        false,
    )
}

pub async fn submit_desktop_sponsored_unshield_self_broadcast(
    request: DesktopSponsoredUnshieldSelfBroadcastRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<DesktopSponsoredSelfBroadcastResult> {
    let (spend_authorization, gas_payer_authorization) = request
        .spend_authorization
        .split_hardware_gas_payment(&request.view_session)?;
    let (sensitive_route, query_rpc_pool) =
        sponsored_sensitive_route(request.chain_id, &request.effective_chain, http, client)?;
    let prepared = prepare_desktop_sponsored_unshield_calldata(
        DesktopSponsoredUnshieldCalldataRequest {
            chain_id: request.chain_id,
            effective_chain: request.effective_chain.clone(),
            view_session: Arc::clone(&request.view_session),
            session: Arc::clone(&request.session),
            vault_store: Arc::clone(&request.vault_store),
            spend_authorization,
            public_account_uuid: request.public_account_uuid.clone(),
            token: request.token,
            amount: request.amount,
            fee_mode: request.fee_mode,
            recipient: request.recipient,
            unwrap: request.unwrap,
            native_top_up: request.native_top_up,
            verify_proof: request.verify_proof,
            gas_fee: request.gas_fee,
            incentive: request.incentive,
            authorization_limit: request.authorization_limit,
            progress_tx: request.progress_tx.clone(),
        },
        http,
        &sensitive_route,
        &query_rpc_pool,
    )
    .await?;
    let activity = sponsored_activity(
        &request.vault_store,
        &request.view_session,
        &prepared,
        vault::ActivityKind::Unshield,
        request.token,
        request.amount,
        vault::ActivityCounterparty::Evm(request.recipient),
    )
    .with_gateway_origin(request.gateway_origin);
    let outcome = submit_prepared_sponsored_self_broadcast(
        DesktopPreparedSponsoredSelfBroadcastRequest {
            transaction_tracking: request.transaction_tracking,
            chain_id: request.chain_id,
            effective_chain: request.effective_chain,
            view_session: request.view_session,
            session: request.session,
            vault_store: request.vault_store,
            gas_payer_authorization,
            vault_password: request.vault_password,
            protected_software_seed_session: request.protected_software_seed_session,
            trezor_pin_matrix_provider: request.trezor_pin_matrix_provider,
            public_account_uuid: request.public_account_uuid,
            prepared: prepared.clone(),
            progress_tx: request.progress_tx,
            command_rx: request.command_rx,
            event_tx: request.event_tx,
        },
        Some(&activity),
        http,
        client,
        sensitive_route,
        query_rpc_pool,
    )
    .await?;
    Ok(DesktopSponsoredSelfBroadcastResult { prepared, outcome })
}
